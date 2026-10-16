//! The furniture map the 3D page edits. Validated on every write, persisted atomically.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

pub const KINDS: &[&str] = &[
    "bed", "sofa", "table", "desk", "chair", "wardrobe", "tv", "door", "window", "corner",
    "router", "other",
];
const MAX_OBJECTS: usize = 500;
const LIMIT_M: f64 = 100.0;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Obj {
    pub id: u64,
    pub kind: String,
    pub x: f64,
    pub z: f64,
    pub yaw: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Data {
    next_id: u64,
    objects: Vec<Obj>,
}

#[derive(Debug)]
pub struct MapStore {
    path: Option<PathBuf>,
    data: Mutex<Data>,
}

fn finite_in(v: Option<f64>, lim: f64) -> Option<f64> {
    v.filter(|x| x.is_finite() && x.abs() <= lim)
}

impl MapStore {
    pub fn open(path: Option<PathBuf>) -> Result<Self, String> {
        let data = match &path {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(t) => {
                    serde_json::from_str::<Data>(&t).map_err(|e| format!("{}: {e}", p.display()))?
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data {
                    next_id: 1,
                    objects: vec![],
                },
                Err(e) => return Err(format!("{}: {e}", p.display())),
            },
            None => Data {
                next_id: 1,
                objects: vec![],
            },
        };
        Ok(Self {
            path,
            data: Mutex::new(Data {
                next_id: data.next_id.max(1),
                ..data
            }),
        })
    }

    fn persist(&self, d: &Data) -> Result<(), String> {
        let Some(p) = &self.path else { return Ok(()) };
        let tmp = p.with_extension("json.tmp");
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        use std::io::Write;
        let text = serde_json::to_vec(d).map_err(|e| e.to_string())?;
        o.open(&tmp)
            .and_then(|mut f| f.write_all(&text).and_then(|()| f.sync_all()))
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, p).map_err(|e| format!("rename {}: {e}", p.display()))
    }

    pub fn snapshot(&self) -> Value {
        let d = self.data.lock().expect("map lock");
        json!({"objects": d.objects, "planes": [], "track": [], "aps": []})
    }

    /// Insert, or update when `id` names an existing object. Returns the id.
    pub fn upsert(&self, body: &Value) -> Result<u64, String> {
        let kind = body
            .get("kind")
            .and_then(Value::as_str)
            .filter(|k| KINDS.contains(k))
            .ok_or("unknown kind")?;
        let x =
            finite_in(body.get("x").and_then(Value::as_f64), LIMIT_M).ok_or("x out of range")?;
        let z =
            finite_in(body.get("z").and_then(Value::as_f64), LIMIT_M).ok_or("z out of range")?;
        let yaw = finite_in(
            body.get("yaw").and_then(Value::as_f64).or(Some(0.0)),
            1000.0,
        )
        .ok_or("yaw invalid")?;
        let mut d = self
            .data
            .lock()
            .map_err(|_| "map lock poisoned".to_string())?;
        let before = (d.next_id, d.objects.clone());
        let id = match body.get("id").and_then(Value::as_u64) {
            Some(id) if d.objects.iter().any(|o| o.id == id) => {
                if let Some(o) = d.objects.iter_mut().find(|o| o.id == id) {
                    *o = Obj {
                        id,
                        kind: kind.into(),
                        x,
                        z,
                        yaw,
                    };
                }
                id
            }
            Some(_) => return Err("no such object".into()),
            None => {
                if d.objects.len() >= MAX_OBJECTS {
                    return Err("object limit reached".into());
                }
                let id = d.next_id;
                d.next_id += 1;
                d.objects.push(Obj {
                    id,
                    kind: kind.into(),
                    x,
                    z,
                    yaw,
                });
                id
            }
        };
        if let Err(e) = self.persist(&d) {
            (d.next_id, d.objects) = before;
            return Err(e);
        }
        Ok(id)
    }

    pub fn delete(&self, id: u64) -> Result<bool, String> {
        let mut d = self
            .data
            .lock()
            .map_err(|_| "map lock poisoned".to_string())?;
        let before = d.objects.clone();
        d.objects.retain(|o| o.id != id);
        let removed = d.objects.len() != before.len();
        if removed {
            if let Err(e) = self.persist(&d) {
                d.objects = before;
                return Err(e);
            }
        }
        Ok(removed)
    }

    pub fn reset(&self) -> Result<(), String> {
        let mut d = self
            .data
            .lock()
            .map_err(|_| "map lock poisoned".to_string())?;
        let before = d.objects.clone();
        d.objects.clear();
        if let Err(e) = self.persist(&d) {
            d.objects = before;
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_roundtrip_and_persistence() {
        let dir = std::env::temp_dir().join(format!("rvcsi-map-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("map.json");
        let m = MapStore::open(Some(p.clone())).unwrap();
        let id = m
            .upsert(&json!({"kind":"bed","x":1.5,"z":-2.0,"yaw":0.5}))
            .unwrap();
        let id2 = m.upsert(&json!({"kind":"router","x":0,"z":0})).unwrap();
        assert_ne!(id, id2);
        assert_eq!(
            m.upsert(&json!({"id":id,"kind":"bed","x":3.0,"z":3.0,"yaw":0}))
                .unwrap(),
            id,
            "update in place"
        );
        assert_eq!(m.snapshot()["objects"].as_array().unwrap().len(), 2);
        let reopened = MapStore::open(Some(p)).unwrap();
        let snap = reopened.snapshot();
        assert_eq!(snap["objects"][0]["x"], 3.0);
        assert_eq!(
            reopened
                .upsert(&json!({"kind":"desk","x":0,"z":0}))
                .unwrap(),
            id2 + 1,
            "ids never reused"
        );
        assert!(reopened.delete(id).unwrap() && !reopened.delete(id).unwrap());
        reopened.reset().unwrap();
        assert!(reopened.snapshot()["objects"]
            .as_array()
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_bad_input() {
        let m = MapStore::open(None).unwrap();
        for bad in [
            json!({"kind":"nuke","x":0,"z":0}),
            json!({"kind":"bed","x":1e9,"z":0}),
            json!({"kind":"bed","x":"1","z":0}),
            json!({"kind":"bed","x":0}),
            json!({"kind":"bed","x":0,"z":0,"id":42}),
            json!({"x":0,"z":0}),
        ] {
            assert!(m.upsert(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn object_count_is_capped() {
        let m = MapStore::open(None).unwrap();
        for _ in 0..MAX_OBJECTS {
            m.upsert(&json!({"kind":"chair","x":0,"z":0})).unwrap();
        }
        assert!(m.upsert(&json!({"kind":"chair","x":0,"z":0})).is_err());
    }

    #[test]
    fn corrupt_state_file_is_an_error_not_silent_reset() {
        let dir = std::env::temp_dir().join(format!("rvcsi-map-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("map.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(MapStore::open(Some(p)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
