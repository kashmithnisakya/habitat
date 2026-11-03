import 'package:habitat/core/core_api.g.dart';

class FakeCoreApi extends CoreApi {
  List<AgentView> agents = [];
  AgentView? fileToInstall;
  Object? listError;
  Object? installError;
  Object? sendError;
  Object? removeError;
  RunResult Function(String text)? reply;
  List<RunEntry> runs = [];
  final List<String> removed = [];
  final List<String> sent = [];

  static AgentView example() => AgentView(
    name: 'hello-agent',
    version: '0.1.0',
    memoryMb: 32,
    runSeconds: 10,
    diskMb: 1,
  );

  @override
  Future<List<AgentView>> listAgents() async {
    if (listError != null) throw listError!;
    return agents;
  }

  @override
  Future<AgentView?> installFromFile() async {
    if (installError != null) throw installError!;
    final agent = fileToInstall;
    if (agent != null) agents = [...agents, agent];
    return agent;
  }

  @override
  Future<AgentView> installExample() async {
    if (installError != null) throw installError!;
    final agent = example();
    agents = [...agents, agent];
    return agent;
  }

  @override
  Future<void> removeAgent(String name) async {
    if (removeError != null) throw removeError!;
    removed.add(name);
    agents = agents.where((agent) => agent.name != name).toList();
  }

  @override
  Future<RunResult> sendMessage(String agent, String text) async {
    sent.add(text);
    if (sendError != null) throw sendError!;
    return reply?.call(text) ??
        RunResult(
          runId: 'abc',
          status: RunStatus.completed,
          messages: ['You said: $text'],
        );
  }

  @override
  Future<List<RunEntry>> recentRuns(String agent) async => runs;
}
