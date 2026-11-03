package io.github.kashmithnisakya.habitat

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.util.Log
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.Executors
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException
import kotlinx.coroutines.CancellableContinuation
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import uniffi.habitat_mobile.AgentInfo
import uniffi.habitat_mobile.CoreException
import uniffi.habitat_mobile.HabitatCore
import uniffi.habitat_mobile.RunReport
import uniffi.habitat_mobile.RunState
import uniffi.habitat_mobile.RunSummary

private const val TAG = "Habitat"
private const val PICK_PACKAGE = 4101
private const val WORKERS = 4
private const val RECENT_RUNS = 50u
private const val EXAMPLE_PACKAGE = "hello-agent.habitat"

// A little over the core's own limit, which is the one that really decides.
private const val MAX_PACKAGE_BYTES = 130L * 1024 * 1024

// Passes requests on to the Rust core, which makes every decision. This holds no policy of its own.
class CoreApiImpl(private val activity: Activity) : CoreApi {
    private val core by lazy {
        HabitatCore.open(File(activity.noBackupFilesDir, "habitat").path)
    }
    private val workers = Executors.newFixedThreadPool(WORKERS).asCoroutineDispatcher()
    private var pendingPick: CancellableContinuation<Uri?>? = null

    override suspend fun listAgents(): List<AgentView> =
        inBackground { core.listAgents().map { it.toView() } }

    override suspend fun installFromFile(): AgentView? {
        val uri = pickFile() ?: return null
        return inBackground {
            val input = activity.contentResolver.openInputStream(uri)
                ?: throw FlutterError("rejected", "That file can't be read.")
            install(input)
        }
    }

    override suspend fun installExample(): AgentView =
        inBackground { install(activity.assets.open(EXAMPLE_PACKAGE)) }

    override suspend fun removeAgent(name: String) =
        inBackground { core.removeAgent(name) }

    override suspend fun sendMessage(agent: String, text: String): RunResult =
        inBackground { core.sendMessage(agent, text).toResult() }

    override suspend fun recentRuns(agent: String): List<RunEntry> =
        inBackground { core.recentRuns(agent, RECENT_RUNS).map { it.toEntry() } }

    fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        if (requestCode != PICK_PACKAGE) return
        val continuation = pendingPick ?: return
        pendingPick = null
        continuation.resume(if (resultCode == Activity.RESULT_OK) data?.data else null)
    }

    // Uses the system's picker, so the app needs no permission to read the user's files.
    private suspend fun pickFile(): Uri? {
        if (pendingPick != null) throw FlutterError("busy", "A file picker is already open.")
        val intent = Intent(Intent.ACTION_OPEN_DOCUMENT)
            .addCategory(Intent.CATEGORY_OPENABLE)
            .setType("*/*")
        return suspendCancellableCoroutine { continuation ->
            pendingPick = continuation
            continuation.invokeOnCancellation { pendingPick = null }
            try {
                @Suppress("DEPRECATION")
                activity.startActivityForResult(intent, PICK_PACKAGE)
            } catch (_: ActivityNotFoundException) {
                pendingPick = null
                continuation.resumeWithException(
                    FlutterError("rejected", "This device has no file picker."),
                )
            }
        }
    }

    // The copy is only ever handed to the core, which treats it as untrusted, and then deleted.
    private fun install(input: InputStream): AgentView {
        val copy = File.createTempFile("incoming", ".habitat", activity.cacheDir)
        try {
            input.use { source ->
                copy.outputStream().use { target -> copyWithLimit(source, target) }
            }
            return core.installPackage(copy.path).toView()
        } finally {
            copy.delete()
        }
    }

    private fun copyWithLimit(source: InputStream, target: OutputStream) {
        val buffer = ByteArray(64 * 1024)
        var total = 0L
        while (true) {
            val count = source.read(buffer)
            if (count < 0) return
            total += count
            if (total > MAX_PACKAGE_BYTES) {
                throw FlutterError("rejected", "That package is too large.")
            }
            target.write(buffer, 0, count)
        }
    }

    private suspend fun <T> inBackground(work: () -> T): T = withContext(workers) {
        try {
            work()
        } catch (error: Throwable) {
            throw toFlutterError(error)
        }
    }

    // Pigeon sends other errors to the UI with a stack trace, which could hold host paths.
    private fun toFlutterError(error: Throwable): FlutterError = when (error) {
        is FlutterError -> error
        is CoreException.Busy -> FlutterError("busy", "The agent is already running.")
        is CoreException.NotFound -> FlutterError("not-found", "That agent isn't installed.")
        is CoreException.Rejected -> FlutterError("rejected", error.reason)
        is CoreException.Failed -> FlutterError("failed", error.reason)
        else -> {
            Log.e(TAG, "The core failed: ${error.javaClass.name}")
            FlutterError("internal", "Something went wrong in the core.")
        }
    }
}

private fun AgentInfo.toView() = AgentView(
    name = name,
    version = version,
    memoryMb = memoryMb?.toLong(),
    runSeconds = runSeconds?.toLong(),
    diskMb = diskMb?.toLong(),
    problem = problem,
)

private fun RunReport.toResult() = RunResult(
    runId = runId,
    status = state.toStatus(),
    detail = detail,
    messages = messages,
)

private fun RunSummary.toEntry() = RunEntry(
    runId = runId,
    status = state.toStatus(),
    detail = detail,
    startedMs = startedMs.toLong(),
    finishedMs = finishedMs?.toLong(),
)

private fun RunState.toStatus() = when (this) {
    RunState.COMPLETED -> RunStatus.COMPLETED
    RunState.FAILED -> RunStatus.FAILED
    RunState.TIME_LIMIT -> RunStatus.TIME_LIMIT
    RunState.MEMORY_LIMIT -> RunStatus.MEMORY_LIMIT
    RunState.CRASHED -> RunStatus.CRASHED
    RunState.INTERNAL -> RunStatus.INTERNAL
    RunState.INTERRUPTED -> RunStatus.INTERRUPTED
    RunState.RUNNING -> RunStatus.RUNNING
}
