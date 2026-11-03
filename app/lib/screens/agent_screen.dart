import 'package:flutter/material.dart';

import '../core/core_api.g.dart';
import '../core/describe.dart';
import 'run_history_screen.dart';

sealed class _Item {
  const _Item(this.text);

  final String text;
}

class _FromUser extends _Item {
  const _FromUser(super.text);
}

class _FromAgent extends _Item {
  const _FromAgent(super.text);
}

class _Notice extends _Item {
  const _Notice(super.text);
}

class AgentScreen extends StatefulWidget {
  const AgentScreen({super.key, required this.api, required this.agent});

  final CoreApi api;
  final AgentView agent;

  @override
  State<AgentScreen> createState() => _AgentScreenState();
}

class _AgentScreenState extends State<AgentScreen> {
  final TextEditingController _input = TextEditingController();
  final ScrollController _scroll = ScrollController();
  final List<_Item> _items = [];
  bool _sending = false;

  @override
  void dispose() {
    _input.dispose();
    _scroll.dispose();
    super.dispose();
  }

  Future<void> _send() async {
    final text = _input.text.trim();
    if (text.isEmpty || _sending) return;
    _input.clear();
    setState(() {
      _items.add(_FromUser(text));
      _sending = true;
    });
    _scrollToEnd();

    final replies = <_Item>[];
    try {
      final result = await widget.api.sendMessage(widget.agent.name, text);
      replies.addAll(result.messages.map(_FromAgent.new));
      if (result.status != RunStatus.completed) {
        replies.add(_Notice(describeRun(result.status, result.detail)));
      }
    } catch (error) {
      replies.add(_Notice(describeError(error)));
    }
    if (!mounted) return;
    setState(() {
      _items.addAll(replies);
      _sending = false;
    });
    _scrollToEnd();
  }

  void _scrollToEnd() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scroll.hasClients) {
        _scroll.jumpTo(_scroll.position.maxScrollExtent);
      }
    });
  }

  Future<void> _confirmRemove() async {
    final name = widget.agent.name;
    final confirmed = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: Text('Remove $name?'),
        content: const Text('This deletes the agent and all of its files.'),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(context).pop(false),
            child: const Text('Cancel'),
          ),
          TextButton(
            onPressed: () => Navigator.of(context).pop(true),
            child: const Text('Remove'),
          ),
        ],
      ),
    );
    if (confirmed != true) return;

    try {
      await widget.api.removeAgent(name);
      if (mounted) Navigator.of(context).pop();
    } catch (error) {
      if (!mounted) return;
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text(describeError(error))));
    }
  }

  void _openHistory() {
    Navigator.of(context).push(
      MaterialPageRoute<void>(
        builder: (_) =>
            RunHistoryScreen(api: widget.api, agentName: widget.agent.name),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final agent = widget.agent;
    final problem = agent.problem;
    return Scaffold(
      appBar: AppBar(
        title: Text(agent.name),
        actions: [
          IconButton(
            tooltip: 'Recent runs',
            icon: const Icon(Icons.history),
            onPressed: _openHistory,
          ),
          IconButton(
            tooltip: 'Remove agent',
            icon: const Icon(Icons.delete_outline),
            onPressed: _confirmRemove,
          ),
        ],
      ),
      body: Column(
        children: [
          Expanded(
            child: problem != null
                ? _Centered(text: "Damaged, so it won't run: $problem")
                : _items.isEmpty
                ? const _Centered(
                    text:
                        "Send a message to start. This conversation isn't "
                        'saved when you leave this screen.',
                  )
                : ListView.builder(
                    controller: _scroll,
                    padding: const EdgeInsets.all(12),
                    itemCount: _items.length,
                    itemBuilder: (context, index) => switch (_items[index]) {
                      final _FromUser item => _UserBubble(text: item.text),
                      final _FromAgent item => _AgentBubble(
                        agentName: agent.name,
                        text: item.text,
                      ),
                      final _Notice item => _NoticeRow(text: item.text),
                    },
                  ),
          ),
          if (_sending) const LinearProgressIndicator(),
          if (problem == null)
            SafeArea(
              top: false,
              child: Padding(
                padding: const EdgeInsets.all(8),
                child: Row(
                  children: [
                    Expanded(
                      child: TextField(
                        controller: _input,
                        enabled: !_sending,
                        minLines: 1,
                        maxLines: 4,
                        textInputAction: TextInputAction.send,
                        onSubmitted: (_) => _send(),
                        decoration: const InputDecoration(
                          hintText: 'Message',
                          border: OutlineInputBorder(),
                        ),
                      ),
                    ),
                    IconButton(
                      tooltip: 'Send',
                      icon: const Icon(Icons.send),
                      onPressed: _sending ? null : _send,
                    ),
                  ],
                ),
              ),
            ),
        ],
      ),
    );
  }
}

class _Centered extends StatelessWidget {
  const _Centered({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(24),
        child: Text(text, textAlign: TextAlign.center),
      ),
    );
  }
}

class _UserBubble extends StatelessWidget {
  const _UserBubble({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return Align(
      alignment: Alignment.centerRight,
      child: Container(
        constraints: const BoxConstraints(maxWidth: 320),
        margin: const EdgeInsets.symmetric(vertical: 4),
        padding: const EdgeInsets.all(12),
        decoration: BoxDecoration(
          color: colors.primaryContainer,
          borderRadius: BorderRadius.circular(12),
        ),
        child: Text(text, style: TextStyle(color: colors.onPrimaryContainer)),
      ),
    );
  }
}

// Plain Text only, with a label added by Habitat, so that an agent's words can't pass as the app's.
class _AgentBubble extends StatelessWidget {
  const _AgentBubble({required this.agentName, required this.text});

  final String agentName;
  final String text;

  @override
  Widget build(BuildContext context) {
    final theme = Theme.of(context);
    final colors = theme.colorScheme;
    return Align(
      alignment: Alignment.centerLeft,
      child: Container(
        constraints: const BoxConstraints(maxWidth: 320),
        margin: const EdgeInsets.symmetric(vertical: 4),
        padding: const EdgeInsets.all(12),
        decoration: BoxDecoration(
          color: colors.surfaceContainerHighest,
          borderRadius: BorderRadius.circular(12),
        ),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text('From $agentName', style: theme.textTheme.labelSmall),
            const SizedBox(height: 4),
            Text(text),
          ],
        ),
      ),
    );
  }
}

class _NoticeRow extends StatelessWidget {
  const _NoticeRow({required this.text});

  final String text;

  @override
  Widget build(BuildContext context) {
    final colors = Theme.of(context).colorScheme;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Icon(Icons.info_outline, size: 18, color: colors.error),
          const SizedBox(width: 8),
          Expanded(
            child: Text(text, style: TextStyle(color: colors.error)),
          ),
        ],
      ),
    );
  }
}
