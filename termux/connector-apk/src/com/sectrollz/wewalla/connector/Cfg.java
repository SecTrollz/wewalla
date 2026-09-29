package com.sectrollz.wewalla.connector;

import android.content.Context;
import android.content.SharedPreferences;

/** Persisted connector settings. */
final class Cfg {
    static final int DEFAULT_PORT = 5077;
    String token = "";
    String host = "127.0.0.1";
    int port = DEFAULT_PORT;
    int rate = 8;                 // connected-AP polls per second
    boolean conn = true, scan = true, rtt = true, imu = true;

    static Cfg load(Context c) {
        SharedPreferences p = c.getSharedPreferences("cfg", Context.MODE_PRIVATE);
        Cfg g = new Cfg();
        g.token = p.getString("token", "");
        g.host = p.getString("host", "127.0.0.1");
        g.port = p.getInt("port", DEFAULT_PORT);
        g.rate = p.getInt("rate", 8);
        g.conn = p.getBoolean("conn", true);
        g.scan = p.getBoolean("scan", true);
        g.rtt = p.getBoolean("rtt", true);
        g.imu = p.getBoolean("imu", true);
        return g;
    }

    void save(Context c) {
        c.getSharedPreferences("cfg", Context.MODE_PRIVATE).edit()
                .putString("token", token).putString("host", host).putInt("port", port)
                .putInt("rate", rate).putBoolean("conn", conn).putBoolean("scan", scan)
                .putBoolean("rtt", rtt).putBoolean("imu", imu).apply();
    }

    /** "conn,scan,rtt,imu" style list; unknown names are ignored. */
    void applyModes(String csv) {
        if (csv == null) return;
        String s = "," + csv.toLowerCase(java.util.Locale.US) + ",";
        conn = s.contains(",conn,");
        scan = s.contains(",scan,");
        rtt = s.contains(",rtt,");
        imu = !s.contains(",noimu,");
    }

    String modes() {
        StringBuilder b = new StringBuilder();
        if (conn) b.append("conn,");
        if (scan) b.append("scan,");
        if (rtt) b.append("rtt,");
        if (imu) b.append("imu,");
        return b.length() == 0 ? "" : b.substring(0, b.length() - 1);
    }
}
