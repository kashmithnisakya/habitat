// To regenerate the code from this, run `dart run pigeon --input pigeons/core_api.dart` in `app/`.
import 'package:pigeon/pigeon.dart';

@ConfigurePigeon(
  PigeonOptions(
    dartOut: 'lib/core/core_api.g.dart',
    kotlinOut: 'android/app/src/main/kotlin/io/github/kashmithnisakya/habitat/CoreApi.g.kt',
    kotlinOptions: KotlinOptions(package: 'io.github.kashmithnisakya.habitat'),
  ),
)
enum RunStatus {
  completed,
  failed,
  timeLimit,
  memoryLimit,
  crashed,
  internal,
  interrupted,
  running,
}

class AgentView {
  AgentView({
    required this.name,
    this.version,
    this.memoryMb,
    this.runSeconds,
    this.diskMb,
    this.problem,
  });

  String name;
  String? version;
  int? memoryMb;
  int? runSeconds;
  int? diskMb;

  // If this is set, the agent is damaged and can only be removed.
  String? problem;
}

class RunResult {
  RunResult({
    required this.runId,
    required this.status,
    this.detail,
    required this.messages,
  });

  String runId;
  RunStatus status;
  String? detail;

  // The agent's own words, which must only ever be shown as plain text.
  List<String> messages;
}

class RunEntry {
  RunEntry({
    required this.runId,
    required this.status,
    this.detail,
    required this.startedMs,
    this.finishedMs,
  });

  String runId;
  RunStatus status;
  String? detail;
  int startedMs;
  int? finishedMs;
}

@HostApi()
abstract class CoreApi {
  @async
  List<AgentView> listAgents();

  // Asks the user to choose a package with the system's picker. Returns null if they cancel.
  @async
  AgentView? installFromFile();

  @async
  AgentView installExample();

  @async
  void removeAgent(String name);

  @async
  RunResult sendMessage(String agent, String text);

  @async
  List<RunEntry> recentRuns(String agent);
}
