import 'package:flutter/material.dart';

import '../core/core_api.g.dart';
import '../core/describe.dart';
import 'agent_screen.dart';

class AgentListScreen extends StatefulWidget {
  const AgentListScreen({super.key, required this.api});

  final CoreApi api;

  @override
  State<AgentListScreen> createState() => _AgentListScreenState();
}

class _AgentListScreenState extends State<AgentListScreen> {
  late Future<List<AgentView>> _agents = widget.api.listAgents();
  bool _installing = false;

  void _reload() {
    final agents = widget.api.listAgents();
    setState(() {
      _agents = agents;
    });
  }

  Future<void> _install(Future<AgentView?> Function() install) async {
    setState(() => _installing = true);
    String? notice;
    try {
      final agent = await install();
      if (agent != null) notice = 'Installed ${agent.name}';
    } catch (error) {
      notice = describeError(error);
    }
    if (!mounted) return;
    setState(() => _installing = false);
    if (notice != null) {
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text(notice)));
    }
    _reload();
  }

  Future<void> _open(AgentView agent) async {
    await Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) => AgentScreen(api: widget.api, agent: agent),
      ),
    );
    if (mounted) _reload();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Habitat')),
      floatingActionButton: FloatingActionButton.extended(
        onPressed: _installing
            ? null
            : () => _install(widget.api.installFromFile),
        icon: const Icon(Icons.add),
        label: Text(_installing ? 'Installing…' : 'Install agent'),
      ),
      body: Column(
        children: [
          const _EarlyDevelopmentWarning(),
          Expanded(
            child: FutureBuilder<List<AgentView>>(
              future: _agents,
              builder: (context, snapshot) {
                if (snapshot.connectionState != ConnectionState.done) {
                  return const Center(child: CircularProgressIndicator());
                }
                if (snapshot.hasError) {
                  return _LoadFailed(
                    message: describeError(snapshot.error!),
                    onRetry: _reload,
                  );
                }
                final agents = snapshot.data!;
                if (agents.isEmpty) {
                  return _NoAgents(
                    onTryExample: _installing
                        ? null
                        : () => _install(widget.api.installExample),
                  );
                }
                return ListView.builder(
                  padding: const EdgeInsets.only(bottom: 88),
                  itemCount: agents.length,
                  itemBuilder: (context, index) => _AgentTile(
                    agent: agents[index],
                    onTap: () => _open(agents[index]),
                  ),
                );
              },
            ),
          ),
        ],
      ),
    );
  }
}

class _EarlyDevelopmentWarning extends StatelessWidget {
  const _EarlyDevelopmentWarning();

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return Container(
      width: double.infinity,
      color: colors.errorContainer,
      padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 8),
      child: Text(
        "Early development: this is not secure, so don't use it to protect "
        'anything.',
        style: TextStyle(color: colors.onErrorContainer),
      ),
    );
  }
}

class _NoAgents extends StatelessWidget {
  const _NoAgents({required this.onTryExample});

  final VoidCallback? onTryExample;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(
              'No agents yet',
              style: Theme.of(context).textTheme.titleLarge,
            ),
            const SizedBox(height: 8),
            const Text(
              'Install a package, or start with the example agent.',
              textAlign: TextAlign.center,
            ),
            const SizedBox(height: 16),
            FilledButton(
              onPressed: onTryExample,
              child: const Text('Try the example agent'),
            ),
          ],
        ),
      ),
    );
  }
}

class _LoadFailed extends StatelessWidget {
  const _LoadFailed({required this.message, required this.onRetry});

  final String message;
  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(message, textAlign: TextAlign.center),
            const SizedBox(height: 16),
            OutlinedButton(onPressed: onRetry, child: const Text('Try again')),
          ],
        ),
      ),
    );
  }
}

class _AgentTile extends StatelessWidget {
  const _AgentTile({required this.agent, required this.onTap});

  final AgentView agent;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final problem = agent.problem;
    return ListTile(
      leading: Icon(problem == null ? Icons.smart_toy_outlined : Icons.warning),
      title: Text(agent.name),
      subtitle: Text(
        problem != null
            ? "Damaged, so it won't run: $problem"
            : 'Version ${agent.version} · up to ${agent.memoryMb} MB and '
                  '${agent.runSeconds} s per run',
      ),
      onTap: onTap,
    );
  }
}
