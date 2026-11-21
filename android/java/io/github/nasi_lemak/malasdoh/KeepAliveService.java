// Keeps MalasDoh's process (and so its SSH connections) running while the
// app is in the background, with the notification Android requires for it:
// "MalasDoh: 2 connections open". Started and stopped by MainActivity as
// connections open and close.
package io.github.nasi_lemak.malasdoh;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;

public class KeepAliveService extends Service {
    static final String COUNT = "connections";
    private static final String CHANNEL = "connections";

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        int n = intent == null ? 1 : intent.getIntExtra(COUNT, 1);
        NotificationManager nm = getSystemService(NotificationManager.class);
        nm.createNotificationChannel(new NotificationChannel(CHANNEL, "Open connections",
                NotificationManager.IMPORTANCE_LOW));
        PendingIntent open = PendingIntent.getActivity(this, 0, new Intent(this, MainActivity.class)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP), PendingIntent.FLAG_IMMUTABLE);
        Notification notification = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(getApplicationInfo().icon)
                .setContentTitle("MalasDoh")
                .setContentText(n == 1 ? "1 connection open" : n + " connections open")
                .setContentIntent(open)
                .setOngoing(true)
                .build();
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else {
            startForeground(1, notification);
        }
        return START_NOT_STICKY;
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
