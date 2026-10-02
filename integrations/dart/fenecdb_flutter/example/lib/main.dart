// A todo list kept in a fenecdb file in the app's documents directory: the
// list is a live query, run again after each write, and the writes are the
// builder's. The platform projects come from `flutter create .` here; CI
// makes them and builds the Android app.
import 'package:fenecdb_flutter/fenecdb_flutter.dart';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final dir = await getApplicationDocumentsDirectory();
  final db = await Fenec.open('${dir.path}/todos.fenec');
  if ((await db.run('collections')).schemas.every((c) => c['name'] != 'todos')) {
    await db.execute('create collection todos (title text, done bool @hash)');
  }
  runApp(MaterialApp(home: Todos(db)));
}

class Todos extends StatelessWidget {
  final Fenec db;
  const Todos(this.db, {super.key});

  @override
  Widget build(BuildContext context) => Scaffold(
        appBar: AppBar(title: const Text('Todos')),
        body: StreamBuilder(
          stream: db.live(db.from('todos').where('done', false).order('title')),
          builder: (context, snap) => ListView(children: [
            for (final t in snap.data ?? const <Map<String, Object?>>[])
              CheckboxListTile(
                title: Text(t['title'] as String),
                value: false,
                onChanged: (_) => db.from('todos').where('id', t['id']).update({'done': true}),
              ),
          ]),
        ),
        floatingActionButton: FloatingActionButton(
          onPressed: () => db.from('todos').insert({'title': 'todo ${DateTime.now().second}', 'done': false}),
          child: const Icon(Icons.add),
        ),
      );
}
