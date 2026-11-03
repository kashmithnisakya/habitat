import 'package:flutter/material.dart';

import '../core/core_api.g.dart';
import '../core/describe.dart';

class RunHistoryScreen extends StatelessWidget {
  const RunHistoryScreen({
    super.key,
    required this.api,
    required this.agentName,
  });

  final CoreApi api;
  final String agentName;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: Text('$agentName: recent runs')),
      body: FutureBuilder<List<RunEntry>>(
        future: api.recentRuns(agentName),
        builder: (context, snapshot) {
          if (snapshot.connectionState != ConnectionState.done) {
            return const Center(child: CircularProgressIndicator());
          }
          if (snapshot.hasError) {
            return Center(child: Text(describeError(snapshot.error!)));
          }
          final runs = snapshot.data!;
          if (runs.isEmpty) {
            return const Center(child: Text('No runs yet.'));
          }
          return ListView.builder(
            itemCount: runs.length,
            itemBuilder: (context, index) {
              final run = runs[index];
              return ListTile(
                title: Text(describeRun(run.status, run.detail)),
                subtitle: Text('${_time(run.startedMs)} · run ${run.runId}'),
              );
            },
          );
        },
      ),
    );
  }
}

String _time(int millis) {
  final time = DateTime.fromMillisecondsSinceEpoch(millis).toString();
  return time.substring(0, 19);
}
