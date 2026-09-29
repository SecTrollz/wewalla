package com.sectrollz.wewalla.connector;

import android.Manifest;
import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;
import android.text.InputType;
import android.view.View;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;
import android.widget.Toast;

import java.util.ArrayList;
import java.util.List;

/**
 * Launcher UI. Also the ONLY way to start sensing: `wewalla connector start` runs
 *   am start -n com.sectrollz.wewalla.connector/.MainActivity --ez autostart true --es token T ...
 * which puts this activity on screen (so Android allows the foreground service to start),
 * starts the service and returns to Termux.
 */
public class MainActivity extends Activity {
    private static final int REQ_PERMS = 42;

    private Cfg cfg;
    private TextView statusView;
    private EditText tokenEt, hostEt, portEt;
    private CheckBox cbConn, cbScan, cbRtt, cbImu;
    private boolean pendingAutostart = false;
    private final Handler ui = new Handler(Looper.getMainLooper());
    private final Runnable ticker = new Runnable() {
        public void run() {
            statusView.setText("Status: " + SensingService.status
                    + (SensingService.running ? "\n(sampling; see notification for live counters)" : ""));
            ui.postDelayed(this, 1000);
        }
    };

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        cfg = Cfg.load(this);
        buildUi();
        handleIntent(getIntent());
    }

    @Override
    protected void onNewIntent(Intent i) {
        super.onNewIntent(i);
        setIntent(i);
        handleIntent(i);
    }

    @Override
    protected void onResume() {
        super.onResume();
        ui.post(ticker);
    }

    @Override
    protected void onPause() {
        ui.removeCallbacks(ticker);
        super.onPause();
    }

    // ------------------------------------------------------------------ intents from Termux

    private void handleIntent(Intent in) {
        if (in == null) return;
        if (in.getBooleanExtra("stop", false)) {
            stopService(new Intent(this, SensingService.class));
            Toast.makeText(this, "Wewalla sensing stopped", Toast.LENGTH_SHORT).show();
            in.removeExtra("stop");
            finish();
            return;
        }
        boolean changed = false;
        if (in.hasExtra("token")) { cfg.token = in.getStringExtra("token").trim(); changed = true; }
        if (in.hasExtra("host")) { cfg.host = in.getStringExtra("host"); changed = true; }
        if (in.hasExtra("port")) { cfg.port = in.getIntExtra("port", Cfg.DEFAULT_PORT); changed = true; }
        if (in.hasExtra("rate")) { cfg.rate = Math.max(1, Math.min(50, in.getIntExtra("rate", 8))); changed = true; }
        if (in.hasExtra("modes")) { cfg.applyModes(in.getStringExtra("modes")); changed = true; }
        if (changed) {
            cfg.save(this);
            syncUi();
        }
        if (in.getBooleanExtra("autostart", false)) {
            in.removeExtra("autostart");
            pendingAutostart = true;
            if (hasLocation()) {
                startSensing();
                finish();                         // hand the screen back to Termux
            } else {
                requestPerms();
            }
        }
    }

    // ------------------------------------------------------------------ permissions

    private boolean hasLocation() {
        return checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION) == PackageManager.PERMISSION_GRANTED;
    }

    private void requestPerms() {
        List<String> l = new ArrayList<String>();
        l.add(Manifest.permission.ACCESS_FINE_LOCATION);
        if (Build.VERSION.SDK_INT >= 33) {
            l.add("android.permission.NEARBY_WIFI_DEVICES");
            l.add("android.permission.POST_NOTIFICATIONS");
        }
        requestPermissions(l.toArray(new String[0]), REQ_PERMS);
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] perms, int[] res) {
        super.onRequestPermissionsResult(code, perms, res);
        if (code != REQ_PERMS) return;
        if (!hasLocation()) {
            Toast.makeText(this, "Location permission is required: Android gates Wi-Fi scan/RTT on it", Toast.LENGTH_LONG).show();
            pendingAutostart = false;
            return;
        }
        if (pendingAutostart) {
            pendingAutostart = false;
            startSensing();
            finish();
        }
    }

    // ------------------------------------------------------------------ actions

    private void readUi() {
        cfg.token = tokenEt.getText().toString().trim();
        cfg.host = hostEt.getText().toString().trim();
        try { cfg.port = Integer.parseInt(portEt.getText().toString().trim()); } catch (NumberFormatException e) { cfg.port = Cfg.DEFAULT_PORT; }
        cfg.conn = cbConn.isChecked();
        cfg.scan = cbScan.isChecked();
        cfg.rtt = cbRtt.isChecked();
        cfg.imu = cbImu.isChecked();
        cfg.save(this);
    }

    private void syncUi() {
        tokenEt.setText(cfg.token);
        hostEt.setText(cfg.host);
        portEt.setText(String.valueOf(cfg.port));
        cbConn.setChecked(cfg.conn);
        cbScan.setChecked(cfg.scan);
        cbRtt.setChecked(cfg.rtt);
        cbImu.setChecked(cfg.imu);
    }

    private void startSensing() {
        Intent s = new Intent(this, SensingService.class);
        startForegroundService(s);
    }

    private void pasteToken() {
        ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        ClipData d = cm.getPrimaryClip();
        if (d != null && d.getItemCount() > 0) {
            tokenEt.setText(d.getItemAt(0).coerceToText(this).toString().trim());
        } else {
            Toast.makeText(this, "Clipboard is empty - run `wewalla pair` in Termux first", Toast.LENGTH_LONG).show();
        }
    }

    // ------------------------------------------------------------------ UI (no XML resources)

    private Button button(String text, View.OnClickListener l) {
        Button b = new Button(this);
        b.setText(text);
        b.setAllCaps(false);
        b.setOnClickListener(l);
        return b;
    }

    private CheckBox check(String text) {
        CheckBox c = new CheckBox(this);
        c.setText(text);
        return c;
    }

    private void buildUi() {
        int p = (int) (16 * getResources().getDisplayMetrics().density);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(p, p, p, p);

        TextView title = new TextView(this);
        title.setText("Wewalla Connector");
        title.setTextSize(22);
        root.addView(title);

        TextView note = new TextView(this);
        note.setText("Feeds Termux with Wi-Fi RSSI / scan / RTT and phone-motion data over localhost. "
                + "This is derived data, not CSI.");
        root.addView(note);

        statusView = new TextView(this);
        statusView.setPadding(0, p / 2, 0, p / 2);
        root.addView(statusView);

        tokenEt = new EditText(this);
        tokenEt.setHint("Pairing token (wewalla pair)");
        tokenEt.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        root.addView(tokenEt);
        root.addView(button("Paste token from clipboard", new View.OnClickListener() {
            public void onClick(View v) { pasteToken(); }
        }));

        hostEt = new EditText(this);
        hostEt.setHint("Host (Termux is 127.0.0.1)");
        root.addView(hostEt);
        portEt = new EditText(this);
        portEt.setHint("Port");
        portEt.setInputType(InputType.TYPE_CLASS_NUMBER);
        root.addView(portEt);

        cbConn = check("Connected-AP RSSI (fast poll)");
        cbScan = check("All-AP scan RSSI");
        cbRtt = check("Wi-Fi RTT ranging (802.11mc APs only)");
        cbImu = check("Phone-motion gating (accelerometer)");
        root.addView(cbConn);
        root.addView(cbScan);
        root.addView(cbRtt);
        root.addView(cbImu);

        root.addView(button("1. Grant permissions", new View.OnClickListener() {
            public void onClick(View v) { requestPerms(); }
        }));
        root.addView(button("2. Start sensing", new View.OnClickListener() {
            public void onClick(View v) {
                readUi();
                if (!hasLocation()) { pendingAutostart = true; requestPerms(); return; }
                startSensing();
            }
        }));
        root.addView(button("Stop sensing", new View.OnClickListener() {
            public void onClick(View v) { stopService(new Intent(MainActivity.this, SensingService.class)); }
        }));
        root.addView(button("Allow background running (battery)", new View.OnClickListener() {
            public void onClick(View v) {
                startActivity(new Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS));
            }
        }));

        TextView tip = new TextView(this);
        tip.setText("Tips: keep Location ON. For faster scans turn OFF Developer options > "
                + "\"Wi-Fi scan throttling\". Keep the phone still while sensing.");
        tip.setPadding(0, p / 2, 0, 0);
        root.addView(tip);

        ScrollView sv = new ScrollView(this);
        sv.addView(root);
        setContentView(sv);
        syncUi();
    }
}
