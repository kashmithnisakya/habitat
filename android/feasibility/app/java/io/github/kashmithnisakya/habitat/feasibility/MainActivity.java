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

        String message;
        try {
            int result = NativeBridge.addInWasm(40, 2);
            message = result == 42
                    ? "PASS: Wasmtime returned 40 + 2 = " + result
                    : "FAIL: Wasmtime returned " + result;
        } catch (Throwable error) {
            message = "FAIL: " + error;
        }

        Log.i(TAG, message);
        TextView view = new TextView(this);
        view.setText(message);
        view.setTextSize(20);
        // Centered because content draws behind the title bar on API 35 and later.
        view.setGravity(Gravity.CENTER);
        setContentView(view);
    }
}
