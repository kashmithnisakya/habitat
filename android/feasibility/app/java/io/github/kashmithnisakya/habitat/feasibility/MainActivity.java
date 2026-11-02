package io.github.kashmithnisakya.habitat.feasibility;

import android.app.Activity;
import android.os.Bundle;
import android.os.Process;
import android.os.SystemClock;
import android.util.Log;
import android.view.Gravity;
import android.widget.TextView;

public final class MainActivity extends Activity {
    private static final String TAG = "HabitatFeasibility";
    private static final long SETTLE_MS = 2000;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        TextView view = new TextView(this);
        view.setTextSize(14);
        // Centered because content draws behind the title bar on API 35 and later.
        view.setGravity(Gravity.CENTER);
        setContentView(view);

        String dir = getFilesDir().getPath();
        String mode = getIntent().getStringExtra("mode");
        String guest = getIntent().getStringExtra("guest");
        if (mode == null || guest == null) {
            showReport(view, runChecks(dir));
            return;
        }

        // On its own thread, because some modes take longer than the main thread is allowed.
        view.setText("Measuring " + mode + " " + guest);
        new Thread(() -> {
            String report = measure(dir, mode, guest);
            runOnUiThread(() -> showReport(view, report));
        }).start();
    }

    private static void showReport(TextView view, String report) {
        for (String line : report.split("\n")) {
            Log.i(TAG, line);
        }
        view.setText(report);
    }

    private static String runChecks(String dir) {
        try {
            return NativeBridge.runChecks(dir) + "\n" + checkAndroidRuntime();
        } catch (Throwable error) {
            return "FAIL: " + error;
        }
    }

    private static String measure(String dir, String mode, String guest) {
        try {
            NativeBridge.load();
            long appMs = SystemClock.elapsedRealtime() - Process.getStartElapsedRealtime();
            // Lets Android finish drawing the screen, so its memory isn't counted as Wasmtime's.
            Thread.sleep(SETTLE_MS);
            return NativeBridge.measure(dir, mode, guest, appMs);
        } catch (Throwable error) {
            return "FAIL: " + error;
        }
    }

    /** Checks that Java null checks and garbage collection still work after Wasmtime has run. */
    private static String checkAndroidRuntime() {
        String missing = null;
        for (int i = 0; i < 100_000; i++) {
            try {
                missing.length();
                return "FAIL AndroidRuntime: null check didn't throw";
            } catch (NullPointerException expected) {
                // Expected on every loop.
            }
        }
        System.gc();
        return "PASS AndroidRuntime: null checks and garbage collection still work";
    }
}
