package com.sectrollz.wewalla.connector;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.content.pm.ServiceInfo;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener;
import android.hardware.SensorManager;
import android.net.wifi.ScanResult;
import android.net.wifi.WifiInfo;
import android.net.wifi.WifiManager;
import android.net.wifi.rtt.RangingRequest;
import android.net.wifi.rtt.RangingResult;
import android.net.wifi.rtt.RangingResultCallback;
import android.net.wifi.rtt.WifiRttManager;
import android.os.Build;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.os.PowerManager;
import android.os.SystemClock;

import org.json.JSONArray;
import org.json.JSONObject;

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executor;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Foreground service that samples what a NON-ROOTED phone can actually observe about Wi-Fi and
 * forwards it to Termux as JSON datagrams on loopback:
 *   conn : RSSI of the connected AP (fast poll, but the framework refreshes it only every ~1-3 s)
 *   scan : RSSI of every visible AP (each result carries the AP's own timestamp = true freshness)
 *   rtt  : 802.11mc round-trip-time ranging (distance + RSSI); each result is a fresh measurement
 *   imu  : accelerometer variability, so the analyzer can ignore data while the phone itself moves
 * None of this is CSI. The Termux side labels it as derived data.
 */
public class SensingService extends Service {
    static volatile String status = "stopped";
    static volatile boolean running = false;

    private static final String CHANNEL = "wewalla_sensing";
    private static final int NOTIF_ID = 7;

    private volatile int gen = 0;          // bump to retire all worker threads
    private Cfg cfg;
    private DatagramSocket sock;
    private InetAddress addr;
    private WifiManager wm;
    private WifiManager.WifiLock wifiLock;
    private PowerManager.WakeLock wakeLock;
    private HandlerThread imuThread;
    private SensorManager sm;
    private SensorEventListener imuListener;
    private final List<Thread> threads = new ArrayList<Thread>();
    private final AtomicLong sent = new AtomicLong();
    private volatile List<ScanResult> lastScan = new ArrayList<ScanResult>();
    private volatile String connSt = "off", scanSt = "off", rttSt = "off", imuSt = "off";

    private final double[] mag = new double[50];
    private int magCount = 0;
    private final Object magLock = new Object();

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        cfg = Cfg.load(this);
        try {
            startInForeground("Starting…");
        } catch (RuntimeException e) {          // missing permission / start not allowed
            status = "cannot start foreground service: " + e.getClass().getSimpleName();
            running = false;
            stopSelf();
            return START_NOT_STICKY;
        }
        stopWork();
        if (cfg.token.length() == 0) {
            status = "no pairing token (run `wewalla pair` in Termux, paste it here)";
            stopSelf();
            return START_NOT_STICKY;
        }
        try {
            startWork();
        } catch (Exception e) {
            status = "start failed: " + e;
            stopSelf();
            return START_NOT_STICKY;
        }
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        stopWork();
        running = false;
        status = "stopped";
        super.onDestroy();
    }

    // ------------------------------------------------------------------ lifecycle

    private Notification buildNotification(String text) {
        return new Notification.Builder(this, CHANNEL)
                .setContentTitle("Wewalla Connector")
                .setContentText(text)
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setOngoing(true)
                .build();
    }

    private void startInForeground(String text) {
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(new NotificationChannel(CHANNEL, "Wewalla sensing",
                NotificationManager.IMPORTANCE_LOW));
        Notification n = buildNotification(text);
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_LOCATION);
        } else {
            startForeground(NOTIF_ID, n);
        }
    }

    private void startWork() throws Exception {
        final int g = ++gen;
        sent.set(0);
        sock = new DatagramSocket();
        addr = InetAddress.getByName(cfg.host);
        wm = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);

        int mode = Build.VERSION.SDK_INT >= 29 ? WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                : WifiManager.WIFI_MODE_FULL_HIGH_PERF;
        wifiLock = wm.createWifiLock(mode, "wewalla");
        wifiLock.setReferenceCounted(false);
        wifiLock.acquire();
        PowerManager pm = (PowerManager) getSystemService(Context.POWER_SERVICE);
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "wewalla:sense");
        wakeLock.setReferenceCounted(false);
        wakeLock.acquire(12L * 60L * 60L * 1000L);

        connSt = cfg.conn ? "starting" : "off";
        scanSt = (cfg.scan || cfg.rtt) ? "starting" : "off";
        rttSt = cfg.rtt ? "starting" : "off";
        imuSt = cfg.imu ? "starting" : "off";

        spawn(g, "hello", new Runnable() { public void run() { helloLoop(g); } });
        if (cfg.conn) spawn(g, "conn", new Runnable() { public void run() { connLoop(g); } });
        if (cfg.scan || cfg.rtt) spawn(g, "scan", new Runnable() { public void run() { scanLoop(g); } });
        if (cfg.rtt) spawn(g, "rtt", new Runnable() { public void run() { rttLoop(g); } });
        if (cfg.imu) startImu(g);
        running = true;
        status = "running → " + cfg.host + ":" + cfg.port;
    }

    private synchronized void stopWork() {
        gen++;
        for (Thread t : threads) t.interrupt();
        for (Thread t : threads) {
            try { t.join(500); } catch (InterruptedException ignored) { }
        }
        threads.clear();
        if (sm != null && imuListener != null) sm.unregisterListener(imuListener);
        imuListener = null;
        if (imuThread != null) { imuThread.quitSafely(); imuThread = null; }
        if (wifiLock != null && wifiLock.isHeld()) wifiLock.release();
        if (wakeLock != null && wakeLock.isHeld()) wakeLock.release();
        if (sock != null) { sock.close(); sock = null; }
    }

    private void spawn(int g, String name, Runnable r) {
        Thread t = new Thread(r, "wewalla-" + name);
        t.setDaemon(true);
        synchronized (threads) { threads.add(t); }
        t.start();
    }

    private boolean alive(int g) {
        return g == gen && !Thread.currentThread().isInterrupted();
    }

    private void sleep(long ms) throws InterruptedException {
        Thread.sleep(ms);
    }

    // ------------------------------------------------------------------ transport

    private JSONObject base(String type) throws Exception {
        JSONObject m = new JSONObject();
        m.put("v", 1);
        m.put("tok", cfg.token);
        m.put("t", type);
        m.put("ts", System.currentTimeMillis());
        return m;
    }

    private void send(JSONObject m) {
        DatagramSocket s = sock;
        if (s == null) return;
        try {
            byte[] b = m.toString().getBytes("UTF-8");
            if (b.length > 60000) return;
            s.send(new DatagramPacket(b, b.length, addr, cfg.port));
            sent.incrementAndGet();
        } catch (Exception ignored) {
            // Termux not listening yet: UDP is fire-and-forget; keep sampling.
        }
    }

    private void sendObs(JSONArray arr) throws Exception {
        if (arr.length() == 0) return;
        JSONObject m = base("obs");
        m.put("o", arr);
        send(m);
    }

    private static String mac(String s) {
        return s == null ? "" : s.toLowerCase(Locale.US);
    }

    // ------------------------------------------------------------------ samplers

    private void helloLoop(int g) {
        int n = 0;
        try {
            while (alive(g)) {
                JSONObject m = base("hello");
                m.put("dev", Build.MANUFACTURER + " " + Build.MODEL);
                m.put("sdk", Build.VERSION.SDK_INT);
                m.put("rtt", getPackageManager().hasSystemFeature(PackageManager.FEATURE_WIFI_RTT));
                m.put("modes", cfg.modes());
                m.put("sent", sent.get());
                m.put("st", "conn=" + connSt + " scan=" + scanSt + " rtt=" + rttSt + " imu=" + imuSt);
                send(m);
                if (++n % 5 == 0) {
                    getSystemService(NotificationManager.class).notify(NOTIF_ID,
                            buildNotification("sent " + sent.get() + " · conn " + connSt + " · scan " + scanSt
                                    + " · rtt " + rttSt));
                }
                sleep(2000);
            }
        } catch (InterruptedException ignored) {
        } catch (Exception e) {
            status = "hello: " + e;
        }
    }

    private void connLoop(int g) {
        long period = Math.max(20L, 1000L / Math.max(1, cfg.rate));
        try {
            while (alive(g)) {
                try {
                    WifiInfo wi = wm.getConnectionInfo();
                    int rssi = wi == null ? -127 : wi.getRssi();
                    if (wi != null && rssi > -127 && rssi < 0) {
                        String id = mac(wi.getBSSID());
                        if (id.length() == 0 || id.equals("02:00:00:00:00:00")) id = "connected"; // location off
                        JSONArray a = new JSONArray();
                        JSONObject o = new JSONObject();
                        o.put("k", "conn");
                        o.put("id", id);
                        o.put("f", wi.getFrequency());
                        o.put("r", rssi);
                        a.put(o);
                        sendObs(a);
                        connSt = "ok";
                    } else {
                        connSt = "not connected";
                    }
                } catch (SecurityException e) {
                    connSt = "no permission";
                }
                sleep(period);
            }
        } catch (InterruptedException ignored) {
        } catch (Exception e) {
            connSt = "error " + e.getClass().getSimpleName();
        }
    }

    private void scanLoop(int g) {
        Map<String, Long> seen = new HashMap<String, Long>();
        long lastStart = 0;
        try {
            while (alive(g)) {
                long now = SystemClock.elapsedRealtime();
                if (now - lastStart >= 3000) {
                    lastStart = now;
                    try { wm.startScan(); } catch (Exception ignored) { } // throttled by Android: cached list still updates
                }
                try {
                    List<ScanResult> rs = wm.getScanResults();
                    lastScan = rs == null ? new ArrayList<ScanResult>() : rs;
                    if (cfg.scan) {
                        JSONArray a = new JSONArray();
                        for (ScanResult r : lastScan) {
                            String id = mac(r.BSSID);
                            Long prev = seen.get(id);
                            if (prev != null && prev.longValue() == r.timestamp) continue; // not re-measured
                            seen.put(id, r.timestamp);
                            JSONObject o = new JSONObject();
                            o.put("k", "scan");
                            o.put("id", id);
                            o.put("f", r.frequency);
                            o.put("r", r.level);
                            o.put("u", r.timestamp);
                            a.put(o);
                        }
                        sendObs(a);
                    }
                    scanSt = lastScan.size() + " APs";
                } catch (SecurityException e) {
                    scanSt = "no location permission";
                }
                sleep(1000);
            }
        } catch (InterruptedException ignored) {
        } catch (Exception e) {
            scanSt = "error " + e.getClass().getSimpleName();
        }
    }

    private void rttLoop(final int g) {
        try {
            WifiRttManager rtt = (WifiRttManager) getSystemService(Context.WIFI_RTT_RANGING_SERVICE);
            if (rtt == null || !getPackageManager().hasSystemFeature(PackageManager.FEATURE_WIFI_RTT)) {
                rttSt = "unsupported by this phone";
                return;
            }
            Executor ex = Executors.newSingleThreadExecutor();
            while (alive(g)) {
                if (!rtt.isAvailable()) {
                    rttSt = "Wi-Fi/location off";
                    sleep(3000);
                    continue;
                }
                List<ScanResult> targets = new ArrayList<ScanResult>();
                for (ScanResult r : lastScan) if (r.is80211mcResponder()) targets.add(r);
                Collections.sort(targets, new Comparator<ScanResult>() {
                    public int compare(ScanResult a, ScanResult b) { return b.level - a.level; }
                });
                int max = RangingRequest.getMaxPeers();
                if (targets.size() > max) targets = new ArrayList<ScanResult>(targets.subList(0, max));
                if (targets.isEmpty()) {
                    rttSt = "no 802.11mc APs in range";
                    sleep(4000);
                    continue;
                }
                final Map<String, Integer> freqs = new HashMap<String, Integer>();
                for (ScanResult r : targets) freqs.put(mac(r.BSSID), r.frequency);
                final CountDownLatch done = new CountDownLatch(1);
                try {
                    rtt.startRanging(new RangingRequest.Builder().addAccessPoints(targets).build(), ex,
                            new RangingResultCallback() {
                                @Override
                                public void onRangingFailure(int code) {
                                    rttSt = "ranging failed (" + code + ")";
                                    done.countDown();
                                }

                                @Override
                                public void onRangingResults(List<RangingResult> results) {
                                    try {
                                        JSONArray a = new JSONArray();
                                        for (RangingResult r : results) {
                                            if (r.getStatus() != RangingResult.STATUS_SUCCESS || r.getMacAddress() == null)
                                                continue;
                                            String id = mac(r.getMacAddress().toString());
                                            JSONObject o = new JSONObject();
                                            o.put("k", "rtt");
                                            o.put("id", id);
                                            Integer f = freqs.get(id);
                                            o.put("f", f == null ? 0 : f.intValue());
                                            o.put("d", r.getDistanceMm());
                                            o.put("s", r.getDistanceStdDevMm());
                                            o.put("r", r.getRssi());
                                            o.put("n", r.getNumSuccessfulMeasurements());
                                            a.put(o);
                                        }
                                        sendObs(a);
                                        rttSt = "ok (" + a.length() + "/" + results.size() + ")";
                                    } catch (Exception e) {
                                        rttSt = "error " + e.getClass().getSimpleName();
                                    }
                                    done.countDown();
                                }
                            });
                } catch (SecurityException e) {
                    rttSt = "no permission";
                    sleep(5000);
                    continue;
                }
                done.await(6, TimeUnit.SECONDS);
                sleep(200);
            }
        } catch (InterruptedException ignored) {
        } catch (Exception e) {
            rttSt = "error " + e.getClass().getSimpleName();
        }
    }

    private void startImu(final int g) {
        sm = (SensorManager) getSystemService(Context.SENSOR_SERVICE);
        Sensor acc = sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER);
        if (acc == null) {
            imuSt = "no accelerometer";
            return;
        }
        imuThread = new HandlerThread("wewalla-imu");
        imuThread.start();
        imuListener = new SensorEventListener() {
            public void onSensorChanged(SensorEvent e) {
                double v = Math.sqrt(e.values[0] * e.values[0] + e.values[1] * e.values[1] + e.values[2] * e.values[2]);
                synchronized (magLock) {
                    mag[magCount % mag.length] = v;
                    magCount++;
                }
            }

            public void onAccuracyChanged(Sensor s, int a) { }
        };
        sm.registerListener(imuListener, acc, SensorManager.SENSOR_DELAY_GAME, new Handler(imuThread.getLooper()));
        spawn(g, "imu", new Runnable() {
            public void run() {
                try {
                    while (alive(g)) {
                        double sd = -1;
                        synchronized (magLock) {
                            int n = Math.min(magCount, mag.length);
                            if (n >= 10) {
                                double mean = 0;
                                for (int i = 0; i < n; i++) mean += mag[i];
                                mean /= n;
                                double var = 0;
                                for (int i = 0; i < n; i++) var += (mag[i] - mean) * (mag[i] - mean);
                                sd = Math.sqrt(var / n);
                            }
                        }
                        if (sd >= 0) {
                            JSONObject m = base("imu");
                            m.put("m", sd);
                            send(m);
                            imuSt = "ok";
                        }
                        sleep(500);
                    }
                } catch (InterruptedException ignored) {
                } catch (Exception e) {
                    imuSt = "error " + e.getClass().getSimpleName();
                }
            }
        });
    }
}
