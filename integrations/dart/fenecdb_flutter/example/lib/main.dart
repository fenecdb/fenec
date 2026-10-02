// A todo list kept in a fenecdb file in the app's own directory: the list
// is a live query, run again after each write, and the writes are the
// builder's. The Dart tab of site/content/docs/mobile.html, as it is
// written there. The platform projects come from `flutter create .` here;
// CI makes them and builds the app for Android and the iOS simulator.
import 'package:fenecdb_flutter/fenecdb_flutter.dart';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';

Future<void> main() async {
  WidgetsFlutterBinding.ensureInitialized();
  final dir = await getApplicationSupportDirectory();
  final db = await Fenec.open('${dir.path}/todos.fenec');
  await db.execute('create collection if not exists todos (title text, done bool @hash)');
  runApp(MaterialApp(home: Todos(db)));
}

class Todos extends StatelessWidget {
  final Fenec db;
  const Todos(this.db, {super.key});

  @override
  Widget build(BuildContext context) => Scaffold(
        body: StreamBuilder(
          stream: db.live(db.from('todos').where('done', false)),
          builder: (context, snap) => ListView(children: [
            for (final todo in snap.data ?? const <Map<String, Object?>>[])
              ListTile(
                title: Text(todo['title'] as String),
                onTap: () => db.from('todos').where('id', todo['id']).update({'done': true}),
              ),
          ]),
        ),
        floatingActionButton: FloatingActionButton(
          onPressed: () => db.from('todos').insert({'title': 'milk', 'done': false}),
          child: const Icon(Icons.add),
        ),
      );
}
