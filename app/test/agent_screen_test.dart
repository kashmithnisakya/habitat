import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:habitat/core/core_api.g.dart';
import 'package:habitat/screens/agent_list_screen.dart';
import 'package:habitat/screens/agent_screen.dart';

import 'fake_core_api.dart';

Future<void> open(WidgetTester tester, FakeCoreApi api, [AgentView? agent]) {
  return tester.pumpWidget(
    MaterialApp(
      home: AgentScreen(api: api, agent: agent ?? FakeCoreApi.example()),
    ),
  );
}

Future<void> send(WidgetTester tester, String text) async {
  await tester.enterText(find.byType(TextField), text);
  await tester.tap(find.byTooltip('Send'));
  await tester.pumpAndSettle();
}

void main() {
  testWidgets('shows the message and the labelled reply', (tester) async {
    final api = FakeCoreApi();
    await open(tester, api);
    await send(tester, 'hello');

    expect(api.sent, ['hello']);
    expect(find.text('hello'), findsOneWidget);
    expect(find.text('You said: hello'), findsOneWidget);
    expect(find.text('From hello-agent'), findsOneWidget);
    expect(find.byType(LinearProgressIndicator), findsNothing);
  });

  testWidgets('does not send an empty message', (tester) async {
    final api = FakeCoreApi();
    await open(tester, api);
    await send(tester, '   ');
    expect(api.sent, isEmpty);
  });

  testWidgets('shows an agent\'s markup as plain text', (tester) async {
    const hostile = '<b>Approved</b> **[tap here](https://example.com)**';
    final api = FakeCoreApi()
      ..reply = (_) => RunResult(
        runId: 'abc',
        status: RunStatus.completed,
        messages: [hostile],
      );
    await open(tester, api);
    await send(tester, 'hi');
    expect(find.text(hostile), findsOneWidget);
  });

  testWidgets('labels a failure message as the agent\'s own claim', (
    tester,
  ) async {
    final api = FakeCoreApi()
      ..reply = (_) => RunResult(
        runId: 'abc',
        status: RunStatus.failed,
        detail: 'disk is full',
        messages: [],
      );
    await open(tester, api);
    await send(tester, 'hi');
    expect(
      find.text('The agent reported a failure. Its own words: disk is full'),
      findsOneWidget,
    );
  });

  testWidgets('shows what an agent said before it crashed', (tester) async {
    final api = FakeCoreApi()
      ..reply = (_) => RunResult(
        runId: 'abc',
        status: RunStatus.crashed,
        detail: 'unreachable',
        messages: ['last words'],
      );
    await open(tester, api);
    await send(tester, 'hi');
    expect(find.text('last words'), findsOneWidget);
    expect(find.text('The agent crashed: unreachable'), findsOneWidget);
  });

  for (final (status, text) in [
    (RunStatus.timeLimit, 'Ran out of time and was stopped'),
    (RunStatus.memoryLimit, 'Ran out of memory'),
  ]) {
    testWidgets('explains $status', (tester) async {
      final api = FakeCoreApi()
        ..reply = (_) => RunResult(runId: 'abc', status: status, messages: []);
      await open(tester, api);
      await send(tester, 'hi');
      expect(find.text(text), findsOneWidget);
    });
  }

  testWidgets('shows an error and lets the user send again', (tester) async {
    final api = FakeCoreApi()
      ..sendError = PlatformException(
        code: 'busy',
        message: 'The agent is already running.',
      );
    await open(tester, api);
    await send(tester, 'first');
    expect(find.text('The agent is already running.'), findsOneWidget);

    api.sendError = null;
    await send(tester, 'second');
    expect(find.text('You said: second'), findsOneWidget);
  });

  testWidgets('a damaged agent can be removed but not messaged', (
    tester,
  ) async {
    final api = FakeCoreApi();
    await open(tester, api, AgentView(name: 'broken', problem: 'changed'));
    expect(find.byType(TextField), findsNothing);
    expect(find.textContaining('changed'), findsOneWidget);
    expect(find.byTooltip('Remove agent'), findsOneWidget);
  });

  testWidgets('nothing is removed until the user confirms', (tester) async {
    final api = FakeCoreApi()..agents = [FakeCoreApi.example()];
    await tester.pumpWidget(MaterialApp(home: AgentListScreen(api: api)));
    await tester.pumpAndSettle();
    await tester.tap(find.text('hello-agent'));
    await tester.pumpAndSettle();

    await tester.tap(find.byTooltip('Remove agent'));
    await tester.pumpAndSettle();
    expect(find.text('Remove hello-agent?'), findsOneWidget);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(api.removed, isEmpty);

    await tester.tap(find.byTooltip('Remove agent'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Remove'));
    await tester.pumpAndSettle();
    expect(api.removed, ['hello-agent']);
    expect(find.text('No agents yet'), findsOneWidget);
  });

  testWidgets('stays on the screen if removing fails', (tester) async {
    final api = FakeCoreApi()
      ..removeError = PlatformException(
        code: 'busy',
        message: 'The agent is already running.',
      );
    await open(tester, api);
    await tester.tap(find.byTooltip('Remove agent'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Remove'));
    await tester.pumpAndSettle();
    expect(find.text('The agent is already running.'), findsOneWidget);
    expect(find.byType(TextField), findsOneWidget);
  });

  testWidgets('recent runs show what happened', (tester) async {
    final api = FakeCoreApi()
      ..runs = [
        RunEntry(runId: 'run1', status: RunStatus.interrupted, startedMs: 1000),
        RunEntry(
          runId: 'run2',
          status: RunStatus.completed,
          startedMs: 500,
          finishedMs: 900,
        ),
      ];
    await open(tester, api);
    await tester.tap(find.byTooltip('Recent runs'));
    await tester.pumpAndSettle();
    expect(find.text('Interrupted before it finished'), findsOneWidget);
    expect(find.text('Completed'), findsOneWidget);
    expect(find.textContaining('run run1'), findsOneWidget);
  });

  testWidgets('says when there are no runs yet', (tester) async {
    await open(tester, FakeCoreApi());
    await tester.tap(find.byTooltip('Recent runs'));
    await tester.pumpAndSettle();
    expect(find.text('No runs yet.'), findsOneWidget);
  });
}
