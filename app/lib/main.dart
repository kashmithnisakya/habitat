import 'package:flutter/material.dart';

import 'core/core_api.g.dart';
import 'screens/agent_list_screen.dart';

void main() {
  runApp(HabitatApp(api: CoreApi()));
}

class HabitatApp extends StatelessWidget {
  const HabitatApp({super.key, required this.api});

  final CoreApi api;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Habitat',
      theme: ThemeData(colorSchemeSeed: Colors.teal),
      home: AgentListScreen(api: api),
    );
  }
}
