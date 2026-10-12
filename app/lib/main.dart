import 'dart:async';

import 'package:app/src/agent_state.dart';
import 'package:app/src/config_page.dart';
import 'package:app/src/log_page.dart';
import 'package:app/src/preview_feed.dart';
import 'package:app/src/preview_page.dart';
import 'package:app/src/rust/frb_generated.dart';
import 'package:flutter/material.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  await RustLib.init();
  runApp(const MyApp());
}

/// Root of the operator tool; owns the single [AgentState] and the one preview feed.
class MyApp extends StatefulWidget {
  const MyApp({super.key});

  @override
  State<MyApp> createState() => _MyAppState();
}

class _MyAppState extends State<MyApp> {
  final AgentState _state = AgentState();
  late final PreviewFeed _preview = PreviewFeed(
    onError: (error) => _state.addLog('preview stream error: $error'),
  );

  @override
  void initState() {
    super.initState();
    // Subscribed for the whole life of the app, and wired to the state that decides which frames
    // may be shown: the picture stays live while the operator is on another page, so switching back
    // to it is instant.
    _preview.start(_state);
    unawaited(_state.load());
  }

  @override
  void dispose() {
    // The feed goes first: it listens to the state.
    _preview.dispose();
    _state.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'SC Webcam',
      debugShowCheckedModeBanner: false,
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: HomeShell(state: _state, preview: _preview),
    );
  }
}

/// The three pages of the tool: the live picture, the event log, the configuration.
class HomeShell extends StatefulWidget {
  const HomeShell({super.key, required this.state, required this.preview});

  final AgentState state;
  final PreviewFeed preview;

  @override
  State<HomeShell> createState() => _HomeShellState();
}

class _HomeShellState extends State<HomeShell> {
  int _index = 0;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: Text(switch (_index) {
          0 => 'SC Webcam',
          1 => 'Event log',
          _ => 'Configuration',
        }),
      ),
      // All three stay mounted, so the log keeps its scroll position and the picture keeps
      // updating while another page is in front.
      body: IndexedStack(
        index: _index,
        children: [
          PreviewPage(state: widget.state, preview: widget.preview),
          LogPage(state: widget.state),
          ConfigPage(state: widget.state),
        ],
      ),
      bottomNavigationBar: NavigationBar(
        selectedIndex: _index,
        onDestinationSelected: (index) => setState(() => _index = index),
        destinations: const [
          NavigationDestination(
            icon: Icon(Icons.videocam_outlined),
            selectedIcon: Icon(Icons.videocam),
            label: 'Preview',
          ),
          NavigationDestination(
            icon: Icon(Icons.receipt_long_outlined),
            selectedIcon: Icon(Icons.receipt_long),
            label: 'Event log',
          ),
          NavigationDestination(
            icon: Icon(Icons.settings_outlined),
            selectedIcon: Icon(Icons.settings),
            label: 'Configuration',
          ),
        ],
      ),
    );
  }
}
