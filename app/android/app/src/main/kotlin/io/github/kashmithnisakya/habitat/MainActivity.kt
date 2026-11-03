package io.github.kashmithnisakya.habitat

import android.content.Intent
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine

class MainActivity : FlutterActivity() {
    private var coreApi: CoreApiImpl? = null

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)
        val api = CoreApiImpl(this)
        coreApi = api
        CoreApi.setUp(flutterEngine.dartExecutor.binaryMessenger, api)
    }

    @Deprecated("FlutterActivity is not a ComponentActivity, so this is how it gets results")
    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        coreApi?.onActivityResult(requestCode, resultCode, data)
    }
}
