// Notes in a fenecdb file in the app's own directory. The list is a live
// query, run again after every write; the search box ranks by the words
// and the toy vector together (lib/notes.dart).
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:path_provider/path_provider.dart';

import 'notes.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final dir = await getApplicationSupportDirectory();
  final notes = await Notes.open('${dir.path}/notes.fenec', await rootBundle.loadString('schema.fenecql'));
  runApp(MaterialApp(title: 'Notes', home: NotesPage(notes)));
}

class NotesPage extends StatefulWidget {
  final Notes notes;
  const NotesPage(this.notes, {super.key});

  @override
  State<NotesPage> createState() => _NotesPageState();
}

class _NotesPageState extends State<NotesPage> {
  static const tags = ['home', 'work', 'travel', 'reading', 'shopping'];
  String? tag;
  bool openOnly = false;
  String words = '';

  Notes get notes => widget.notes;

  // A new stream when the filters change; the same one between writes.
  late Stream<List<Map<String, Object?>>> rows = _query();

  Stream<List<Map<String, Object?>>> _query() =>
      notes.live(words.isEmpty ? notes.list(tag: tag, openOnly: openOnly) : notes.search(words));

  void _set(VoidCallback change) => setState(() {
    change();
    rows = _query();
  });

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: const Text('Notes')),
    body: Column(
      children: [
        Padding(
          padding: const EdgeInsets.all(8),
          child: TextField(
            decoration: const InputDecoration(prefixIcon: Icon(Icons.search), hintText: 'Search'),
            onChanged: (v) => _set(() => words = v.trim()),
          ),
        ),
        Wrap(
          spacing: 6,
          children: [
            FilterChip(label: const Text('open'), selected: openOnly, onSelected: (v) => _set(() => openOnly = v)),
            for (final t in tags)
              FilterChip(label: Text(t), selected: tag == t, onSelected: (v) => _set(() => tag = v ? t : null)),
          ],
        ),
        Expanded(
          child: StreamBuilder(
            stream: rows,
            builder: (context, snap) {
              if (snap.hasError) return Center(child: Text('${snap.error}'));
              return ListView(
                children: [
                  for (final n in snap.data ?? const <Map<String, Object?>>[])
                    ListTile(
                      title: Text(n['title'] as String? ?? ''),
                      subtitle: Text('${n['body']}\n${(n['tags'] as List?)?.join(', ') ?? ''}'),
                      isThreeLine: true,
                      trailing: Icon(n['done'] == true ? Icons.check_box : Icons.check_box_outline_blank),
                      onTap: () => notes.finish(n['id'] as int),
                    ),
                ],
              );
            },
          ),
        ),
      ],
    ),
    floatingActionButton: FloatingActionButton(onPressed: _add, child: const Icon(Icons.add)),
  );

  Future<void> _add() async {
    final title = TextEditingController(), body = TextEditingController(), tagText = TextEditingController();
    final ok = await showDialog<bool>(
      context: context,
      builder: (context) => AlertDialog(
        title: const Text('New note'),
        content: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            TextField(
              controller: title,
              decoration: const InputDecoration(labelText: 'Title'),
            ),
            TextField(
              controller: body,
              decoration: const InputDecoration(labelText: 'Body'),
            ),
            TextField(
              controller: tagText,
              decoration: const InputDecoration(labelText: 'Tags, comma separated'),
            ),
          ],
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(context, false), child: const Text('Cancel')),
          FilledButton(onPressed: () => Navigator.pop(context, true), child: const Text('Add')),
        ],
      ),
    );
    if (ok != true || title.text.trim().isEmpty) return;
    final t = [
      for (final s in tagText.text.split(','))
        if (s.trim().isNotEmpty) s.trim(),
    ];
    // The live list shows it: nothing here redraws.
    await notes.add(title.text.trim(), body.text.trim(), t);
  }
}
