// Written by site/builder_tabs.py (make docs-builder): do not edit.

// The code of each builder tab on the site's FenecQL pages, as the page
// shows it, run against a transport that records and held to the statement
// its case of integrations/builder-golden.json makes: a tab shows code this
// SDK runs. make docs-builder writes the file again as the pages' tabs
// change, and site/builder_tabs.py --check fails while it is behind them.

import 'dart:convert';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

/// What the tabs call `db`: queries bound to a recorder, as a database's are to its engine.
class _Recording {
  ({String text, List<Object?> params})? sent;

  Query from(String collection) => Query.from(collection).bind((text, params) async {
        sent = (text: text, params: params);
        if (RegExp(r'^(put|set|del) ').hasMatch(text)) return Answer.of({'kind': 'affected', 'count': 0});
        return Answer.of({
          'kind': 'rows',
          'result': {
            'columns': [],
            'rows': text.endsWith(' count')
                ? [
                    {'count': 0}
                  ]
                : []
          }
        });
      });

  void sends(String text, String params) {
    expect(sent?.text, text);
    final got = jsonDecode(jsonEncode(sent?.params));
    expect(_same(got, jsonDecode(params)), isTrue, reason: 'params $got, want $params');
  }
}

/// JSON compared by value: a number written 1 and 1.0 is one number.
bool _same(Object? a, Object? b) {
  if (a is num && b is num) return a.toDouble() == b.toDouble();
  if (a is List && b is List) {
    return a.length == b.length && [for (var i = 0; i < a.length; i++) i].every((i) => _same(a[i], b[i]));
  }
  if (a is Map && b is Map) return a.length == b.length && a.keys.every((k) => b.containsKey(k) && _same(a[k], b[k]));
  return a == b;
}

void main() {
  test('docs: aggregates over every order', () async {
    final db = _Recording();
    await db.from('orders').select(['count(*)', 'sum(total)', 'avg(total)', 'min(at)', 'max(at)']).rows();
    db.sends('get orders select count(*), sum(total), avg(total), min(at), max(at)', '[]');
  });

  test('docs: aggregates of each status', () async {
    final db = _Recording();
    await db.from('orders').select(['status', 'count(*)', 'avg(total)']).where('year', 2026).group('status').rows();
    db.sends('get orders select status, count(*), avg(total) where year = \$1 group status', '[2026]');
  });

  test('docs: the ten customers who spent most', () async {
    final db = _Recording();
    await db
        .from('orders')
        .select(['customer', 'sum(total)'])
        .group('customer')
        .order('sum(total)', 'desc')
        .limit(10)
        .rows();
    db.sends('get orders select customer, sum(total) group customer order sum(total) desc limit \$1', '[10]');
  });

  test('docs: distinct users an hour and event', () async {
    final db = _Recording();
    await db
        .from('events')
        .select([Computed.bucket('at', '1h').as('hour'), 'name', Computed.countDistinct('user').as('users')]).group(
            ['hour', 'name']).rows();
    db.sends('get events select bucket(at, 1h) as hour, name, count(distinct user) as users group hour, name', '[]');
  });

  test('docs: one-minute bars of a symbol', () async {
    final db = _Recording();
    await db
        .from('ticks')
        .select([
          Computed.bucket('at', '1m').as('bar'),
          Computed.first('px', 'at').as('open'),
          Computed.expr('max(px)').as('high'),
          Computed.expr('min(px)').as('low'),
          Computed.last('px', 'at').as('close'),
          Computed.expr('sum(qty)').as('volume')
        ])
        .where('sym', 'S07')
        .group('bar')
        .rows();
    db.sends(
        'get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = \$1 group bar',
        '["S07"]');
  });

  test('docs: vwap of each symbol', () async {
    final db = _Recording();
    await db.from('ticks').select(['sym', Computed.expr('sum(px * qty) / sum(qty)').as('vwap')]).group('sym').rows();
    db.sends('get ticks select sym, sum(px * qty) / sum(qty) as vwap group sym', '[]');
  });

  test('docs: totals of a json path', () async {
    final db = _Recording();
    await db.from('events').select(['props.plan', 'sum(props.v)']).group('props.plan').rows();
    db.sends('get events select props.plan, sum(props.v) group props.plan', '[]');
  });

  test('docs: the three statuses that took most', () async {
    final db = _Recording();
    await db
        .from('orders')
        .select(['status', 'count(*)', 'sum(total)'])
        .where('year', 2026)
        .group('status')
        .order('sum(total)', 'desc')
        .limit(3)
        .rows();
    db.sends(
        'get orders select status, count(*), sum(total) where year = \$1 group status order sum(total) desc limit \$2',
        '[2026, 3]');
  });

  test('docs: one-minute bars since a time', () async {
    final db = _Recording();
    await db
        .from('ticks')
        .select([
          Computed.bucket('at', '1m').as('bar'),
          Computed.first('px', 'at').as('open'),
          Computed.expr('max(px)').as('high'),
          Computed.expr('min(px)').as('low'),
          Computed.last('px', 'at').as('close'),
          Computed.expr('sum(qty)').as('volume')
        ])
        .where('sym', 'S07')
        .where('at', '>=', '2026-05-03T09:00:00Z')
        .group('bar')
        .rows();
    db.sends(
        'get ticks select bucket(at, 1m) as bar, first(px by at) as open, max(px) as high, min(px) as low, last(px by at) as close, sum(qty) as volume where sym = \$1 and at >= \$2 group bar',
        '["S07", "2026-05-03T09:00:00Z"]');
  });

  test('docs: the newest articles', () async {
    final db = _Recording();
    await db.from('articles').order('published', 'desc').limit(20).rows();
    db.sends('get articles order published desc limit \$1', '[20]');
  });

  test('docs: the newest articles since a day', () async {
    final db = _Recording();
    await db.from('articles').where('published', '>=', '2026-09-01').order('published', 'desc').limit(20).rows();
    db.sends('get articles where published >= \$1 order published desc limit \$2', '["2026-09-01", 20]');
  });

  test('docs: articles counted over a range of titles', () async {
    final db = _Recording();
    await db.from('articles').where('title', '>=', 'k').where('title', '<', 'l').count();
    db.sends('get articles where title >= \$1 and title < \$2 count', '["k", "l"]');
  });

  test('docs: the nearest articles since a day', () async {
    final db = _Recording();
    await db.from('articles').where('published', '>=', '2026-01-01').near('embed', [0.9, 0.1, 0]).limit(10).rows();
    db.sends('get articles where published >= \$1 near embed \$2 limit \$3', '["2026-01-01", [0.9, 0.1, 0], 10]');
  });

  test('docs: products with their newest reviews', () async {
    final db = _Recording();
    await db
        .from('products')
        .limit(20)
        .lookup('reviews', on: 'product_id', order: [SortKey('created', 'desc')], limit: 3)
        .rows();
    db.sends('get products limit \$1 lookup reviews on product_id order created desc limit \$2', '[20, 3]');
  });

  test('docs: shops, their orders and the orders\' lines', () async {
    final db = _Recording();
    await db
        .from('shops')
        .limit(20)
        .lookup('orders', on: 'shop_id', limit: 3)
        .lookup('lines', on: 'order_id', limit: 5)
        .rows();
    db.sends('get shops limit \$1 lookup orders on shop_id limit \$2 lookup lines on order_id limit \$3', '[20, 3, 5]');
  });

  test('docs: products with a five-star review', () async {
    final db = _Recording();
    await db.from('products').limit(20).lookup('reviews', on: 'product_id', required: true, where: {'stars': 5}).rows();
    db.sends('get products limit \$1 lookup reviews on product_id required where stars = \$2', '[20, 5]');
  });

  test('docs: products with a five-star review counted', () async {
    final db = _Recording();
    await db.from('products').lookup('reviews', on: 'product_id', required: true, where: {'stars': 5}).count();
    db.sends('get products count lookup reviews on product_id required where stars = \$1', '[5]');
  });

  test('docs: articles counted since a year', () async {
    final db = _Recording();
    await db.from('articles').where('year', '>=', 2024).count();
    db.sends('get articles where year >= \$1 count', '[2024]');
  });

  test('docs: articles matching words', () async {
    final db = _Recording();
    await db.from('articles').match('body', 'ownership').limit(10).rows();
    db.sends('get articles match body \$1 limit \$2', '["ownership", 10]');
  });

  test('docs: articles matching words since a year', () async {
    final db = _Recording();
    await db.from('articles').where('year', '>=', 2024).match('body', 'ownership').limit(10).rows();
    db.sends('get articles where year >= \$1 match body \$2 limit \$3', '[2024, "ownership", 10]');
  });

  test('docs: articles matching words reranked', () async {
    final db = _Recording();
    await db
        .from('articles')
        .match('body', 'ownership')
        .rerank('embed', [0.9, 0.1, 0], candidates: 500)
        .limit(10)
        .rows();
    db.sends(
        'get articles match body \$1 rerank embed \$2 candidates 500 limit \$3', '["ownership", [0.9, 0.1, 0], 10]');
  });

  test('docs: matches with their marks', () async {
    final db = _Recording();
    await db.from('articles').select(['title']).highlight('body').match('body', 'ownership').limit(10).rows();
    db.sends('get articles select title, highlight(body) match body \$1 limit \$2', '["ownership", 10]');
  });

  test('docs: matches with their marks tagged', () async {
    final db = _Recording();
    await db
        .from('articles')
        .select(['title'])
        .highlight('body', pre: '<mark>', post: '</mark>')
        .match('body', 'ownership')
        .limit(10)
        .rows();
    db.sends('get articles select title, highlight(body, \$1, \$2) match body \$3 limit \$4',
        '["<mark>", "</mark>", "ownership", 10]');
  });

  test('docs: matches with a snippet', () async {
    final db = _Recording();
    await db
        .from('articles')
        .select(['title'])
        .snippet('body', 20, ellipsis: '…')
        .match('body', 'ownership')
        .limit(10)
        .rows();
    db.sends('get articles select title, snippet(body, 20, \$1) match body \$2 limit \$3', '["…", "ownership", 10]');
  });

  test('docs: matches with a snippet tagged', () async {
    final db = _Recording();
    await db
        .from('articles')
        .select(['title'])
        .snippet('body', 20, ellipsis: '…', pre: '<b>', post: '</b>')
        .match('body', 'ownership')
        .limit(10)
        .rows();
    db.sends('get articles select title, snippet(body, 20, \$1, \$2, \$3) match body \$4 limit \$5',
        '["…", "<b>", "</b>", "ownership", 10]');
  });

  test('docs: tents counted by price range', () async {
    final db = _Recording();
    await db
        .from('products')
        .where('category', 'tents')
        .limit(24)
        .facet('price', ranges: [0, 2500, 5000, 10000, 25000], disjunctive: true)
        .rows();
    db.sends('get products where category = \$1 limit \$2 facet price ranges [0, 2500, 5000, 10000, 25000] disjunctive',
        '["tents", 24]');
  });

  test('docs: the nearest articles', () async {
    final db = _Recording();
    await db.from('articles').near('embed', [0.9, 0.1, 0]).limit(10).rows();
    db.sends('get articles near embed \$1 limit \$2', '[[0.9, 0.1, 0], 10]');
  });

  test('docs: the nearest articles since a year', () async {
    final db = _Recording();
    await db.from('articles').where('year', '>=', 2024).near('embed', [0.9, 0.1, 0], ef: 200).limit(10).rows();
    db.sends('get articles where year >= \$1 near embed \$2 ef 200 limit \$3', '[2024, [0.9, 0.1, 0], 10]');
  });

  test('docs: the nearest articles exactly', () async {
    final db = _Recording();
    await db.from('articles').near('embed', [0.9, 0.1, 0], exact: true).limit(10).rows();
    db.sends('get articles near embed \$1 exact limit \$2', '[[0.9, 0.1, 0], 10]');
  });

  test('docs: the nearest articles filtered', () async {
    final db = _Recording();
    await db
        .from('articles')
        .select(['title', 'year'])
        .where('year', '>=', 2024)
        .where('tags', 'has', 'rust')
        .where(Cond.not(Cond.cmp('title', '~', 'draft')))
        .near('embed', [0.9, 0.1, 0], ef: 128)
        .limit(10)
        .rows();
    db.sends(
        'get articles select title, year where year >= \$1 and tags has \$2 and not (title ~ \$3) near embed \$4 ef 128 limit \$5',
        '[2024, "rust", "draft", [0.9, 0.1, 0], 10]');
  });

  test('docs: a product that must be there at its price', () async {
    final db = _Recording();
    await db.from('products').where('sku', 'tee').where('price', 1200).where('stock', '>', 0).require(1).rows();
    db.sends('get products where sku = \$1 and price = \$2 and stock > \$3 require 1', '["tee", 1200, 0]');
  });

  test('docs: a coupon that must still be usable', () async {
    final db = _Recording();
    await db.from('coupons').where('code', 'X1').where(Cond.raw('uses < max_uses')).limit(1).require(1).rows();
    db.sends('get coupons where code = \$1 and uses < max_uses limit \$2 require 1', '["X1", 1]');
  });

  test('docs: no hold that must be there', () async {
    final db = _Recording();
    await db.from('holds').where('order_ref', 'o-17').where('state', 'held').require(0).rows();
    db.sends('get holds where order_ref = \$1 and state = \$2 require 0', '["o-17", "held"]');
  });

  test('docs: an article set by id', () async {
    final db = _Recording();
    await db.from('articles').where('id', 42).update({'year': 2025});
    db.sends('set articles {year: \$1} where id = \$2', '[2025, 42]');
  });

  test('docs: an article\'s views counted', () async {
    final db = _Recording();
    await db.from('articles').where('id', 42).update({'views': Computed.inc(1)});
    db.sends('set articles {views: coalesce(views, 0) + \$1} where id = \$2', '[1, 42]');
  });

  test('docs: the articles before a year deleted', () async {
    final db = _Recording();
    await db.from('articles').where('year', '<', 2000).delete();
    db.sends('del articles where year < \$1', '[2000]');
  });
}
