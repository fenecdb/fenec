<?php
// PDO (pdo_pgsql) and pgvector-php against fenec-server. Each check prints its
// name; a failure is counted and the run exits non-zero.
require __DIR__ . '/vendor/autoload.php';
use Pgvector\Vector;

$failed = 0;
function check(string $name, callable $body): void {
    global $failed;
    try { $body(); echo "ok   $name\n"; }
    catch (Throwable $e) { $failed++; echo "FAIL $name: " . $e->getMessage() . "\n"; }
}
function expect(bool $cond, string $what): void { if (!$cond) throw new Exception($what); }

$pdo = null;
check('connects', function () use (&$pdo) {
    $pdo = new PDO(getenv('FENEC_PG_PDO'), 'fenec', getenv('FENEC_PG_PASSWORD'),
        [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);
});
check('creates a collection', fn () => $pdo->exec(
    'create collection if not exists php_docs (title text, year int @hash, embed vector<3> @hnsw(cosine))'));
check('writes with bound parameters', function () use ($pdo) {
    $s = $pdo->prepare('put php_docs {title: ?, year: ?, embed: [0.1, 0.2, 0.3]}');
    $s->execute(['Night at the oasis', 2024]);
    expect($s->rowCount() === 1, 'one row written');
});
check('reads rows', function () use ($pdo) {
    $s = $pdo->prepare('get php_docs select title, year where year >= ?');
    $s->execute([2020]);
    $r = $s->fetch(PDO::FETCH_ASSOC);
    expect($r['title'] === 'Night at the oasis' && (int)$r['year'] === 2024, 'its values');
});
check('commits and rolls back', function () use ($pdo) {
    $pdo->beginTransaction();
    $pdo->exec("put php_docs {title: 'kept', year: 2025, embed: [0.3, 0.2, 0.1]}");
    $pdo->commit();
    $pdo->beginTransaction();
    $pdo->exec("put php_docs {title: 'dropped', year: 2025, embed: [0.2, 0.2, 0.2]}");
    $pdo->rollBack();
    $n = $pdo->query("get php_docs where title = 'dropped' count")->fetchColumn();
    expect((int)$n === 0, 'the rolled back row is gone');
});
check('writes a Vector and searches near one', function () use ($pdo) {
    $s = $pdo->prepare("put php_docs {title: 'vector parameter', year: 2026, embed: ?}");
    $s->execute([new Vector([0.1, 0.25, 0.3])]);
    $s = $pdo->prepare('get php_docs select title, embed near embed ? limit 1');
    $s->execute([new Vector([0.1, 0.2, 0.3])]);
    $r = $s->fetch(PDO::FETCH_ASSOC);
    expect($r['title'] === 'Night at the oasis', 'the identical vector is nearest');
    $v = (new Vector($r['embed']))->toArray();
    expect(count($v) === 3 && abs($v[1] - 0.2) < 1e-6, 'the vector as written');
});
echo $failed === 0 ? "all passed\n" : "$failed failed\n";
exit($failed === 0 ? 0 : 1);
