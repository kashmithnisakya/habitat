import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:habitat/core/core_api.g.dart';
import 'package:habitat/core/describe.dart';
import 'package:habitat/main.dart';

import 'fake_core_api.dart';

Future<void> show(WidgetTester tester, FakeCoreApi api) async {
  await tester.pumpWidget(HabitatApp(api: api));
  await tester.pumpAndSettle();
}

void main() {
  testWidgets('always warns that this is not secure', (tester) async {
    await show(tester, FakeCoreApi());
    expect(find.textContaining('this is not secure'), findsOneWidget);
  });

  testWidgets('offers the example agent when there are no agents', (
    tester,
  ) async {
    final api = FakeCoreApi();
    await show(tester, api);
    expect(find.text('No agents yet'), findsOneWidget);

    await tester.tap(find.text('Try the example agent'));
    await tester.pumpAndSettle();

    expect(find.text('hello-agent'), findsOneWidget);
    expect(find.text('Installed hello-agent'), findsOneWidget);
    expect(find.text('No agents yet'), findsNothing);
  });

  testWidgets('shows the limits an agent actually gets', (tester) async {
    await show(tester, FakeCoreApi()..agents = [FakeCoreApi.example()]);
    expect(find.textContaining('up to 32 MB and 10 s'), findsOneWidget);
  });

  testWidgets('installs an agent chosen from a file', (tester) async {
    final api = FakeCoreApi()
      ..fileToInstall = AgentView(
        name: 'picked',
        version: '1.0.0',
        memoryMb: 8,
        runSeconds: 5,
        diskMb: 1,
      );
    await show(tester, api);
    await tester.tap(find.text('Install agent'));
    await tester.pumpAndSettle();
    expect(find.text('picked'), findsOneWidget);
  });

  testWidgets('does nothing when the file picker is cancelled', (tester) async {
    final api = FakeCoreApi();
    await show(tester, api);
    await tester.tap(find.text('Install agent'));
    await tester.pumpAndSettle();
    expect(find.text('No agents yet'), findsOneWidget);
    expect(find.byType(SnackBar), findsNothing);
  });

  testWidgets('shows why a package was refused', (tester) async {
    final api = FakeCoreApi()
      ..installError = PlatformException(
        code: 'rejected',
        message: 'invalid package: it is not a tar file',
      );
    await show(tester, api);
    await tester.tap(find.text('Install agent'));
    await tester.pumpAndSettle();
    expect(find.text('Invalid package: it is not a tar file'), findsOneWidget);
    expect(find.text('No agents yet'), findsOneWidget);
  });

  testWidgets('marks a damaged agent', (tester) async {
    final api = FakeCoreApi()
      ..agents = [AgentView(name: 'broken', problem: 'it has changed')];
    await show(tester, api);
    expect(find.textContaining('Damaged'), findsOneWidget);
    expect(find.textContaining('it has changed'), findsOneWidget);
  });

  testWidgets('says so when the core is not connected, and can retry', (
    tester,
  ) async {
    final api = FakeCoreApi()
      ..listError = PlatformException(code: 'channel-error');
    await show(tester, api);
    expect(find.text(coreNotConnected), findsOneWidget);

    api.listError = null;
    await tester.tap(find.text('Try again'));
    await tester.pumpAndSettle();
    expect(find.text('No agents yet'), findsOneWidget);
  });

  testWidgets('never shows the text of an error it does not recognise', (
    tester,
  ) async {
    final api = FakeCoreApi()..listError = StateError('/data/secret/path');
    await show(tester, api);
    expect(find.text('Something went wrong.'), findsOneWidget);
    expect(find.textContaining('secret'), findsNothing);
  });
}
