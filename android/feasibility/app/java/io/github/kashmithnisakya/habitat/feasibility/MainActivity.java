package io.github.kashmithnisakya.habitat.feasibility;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.view.Gravity;
import android.widget.TextView;

public final class MainActivity extends Activity {
    private static final String TAG = "HabitatFeasibility";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        String report;
        try {
            report = NativeBridge.runChecks() + "\n" + checkAndroidRuntime();
        } catch (Throwable error) {
            report = "FAIL: " + error;
        }

        for (String line : report.split("\n")) {
            Log.i(TAG, line);
        }

        TextView view = new TextView(this);
        view.setText(report);
        view.setTextSize(14);
        // Centered because content draws behind the title bar on API 35 and later.
        view.setGravity(Gravity.CENTER);
        setContentView(view);
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
