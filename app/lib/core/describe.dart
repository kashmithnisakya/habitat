import 'package:flutter/services.dart';

import 'core_api.g.dart';

const String coreNotConnected =
    "The Rust core isn't connected on this device yet.";

String describeError(Object error) {
  if (error is PlatformException) {
    if (error.code == 'channel-error') return coreNotConnected;
    final message = error.message;
    if (message != null && message.isNotEmpty) {
      return message[0].toUpperCase() + message.substring(1);
    }
  }
  return 'Something went wrong.';
}

String describeStatus(RunStatus status) {
  return switch (status) {
    RunStatus.completed => 'Completed',
    RunStatus.failed => 'The agent reported a failure',
    RunStatus.timeLimit => 'Ran out of time and was stopped',
    RunStatus.memoryLimit => 'Ran out of memory',
    RunStatus.crashed => 'The agent crashed',
    RunStatus.internal => "Habitat couldn't run the agent",
    RunStatus.interrupted => 'Interrupted before it finished',
    RunStatus.running => 'Still running',
  };
}

// A failure's detail is labelled when it's the agent's own claim rather than something Habitat saw.
String describeRun(RunStatus status, String? detail) {
  final summary = describeStatus(status);
  if (detail == null || detail.isEmpty) return summary;
  if (status == RunStatus.failed) return "$summary. Its own words: $detail";
  return '$summary: $detail';
}
