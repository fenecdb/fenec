//! Points on the earth: the `geo` type, `distance` and `within`, the `@geo`
//! index, and `near` over a point -- Redis's GEOADD and GEOSEARCH, in a
//! collection.
//!
//! A point is its longitude and latitude in degrees, two `f64`s -- 16 bytes
//! under its own tag in a document, `[lon, lat]` once read -- written and
//! read as `[lon, lat]`: GeoJSON's order, and the order Redis and PostGIS
//! take them in. Kept as given, not rounded to a cell -- Redis keeps a
//! 52-bit geohash, which moves a point to the centre of its cell -- 0.6 m
//! by 0.3 m at the equator -- so its `GEOPOS` gives back other numbers than
//! it was handed.
//!
//! A distance is the haversine on a sphere of Redis's radius, so it is
//! Redis's `GEODIST` -- within what Redis moves its points by. PostGIS
//! over `geography` with `use_spheroid = false` takes the earth's mean
//! radius, 6 371 008.8 m, and gives 0.03% less. The ellipsoid (WGS84,
//! PostGIS's default) differs from a sphere by up to 0.5%, and neither
//! Redis nor a client computing distances by hand uses it.
//!
//! The trigonometry is this module's own, in plain `f64` arithmetic: the
//! standard library's `sin`, `cos` and `asin` are the platform's libm --
//! macOS's, glibc's, musl's and the browser module's port each round their
//! last bit their own way, so a point on a radius's edge could be in on a
//! server and out in a page -- and in the browser module they were libm's
//! range reduction and its tables. Series over the short ranges a distance
//! needs agree on every target, bit for bit.
//!
//! The index keeps a row's point as a 64-bit key, its longitude and
//! latitude each cut to 32 bits (under a centimetre) and interleaved -- a
//! Morton, or Z-order, code -- in the ordered index's chunks
//! ([`crate::sorted`]): a cell of the quadtree over the key's bits is a
//! range of keys. A radius or a box is the cells covering its bounding box,
//! each a range read, each key's own 32-bit coordinates tested against the
//! box, and the rows left tested exactly; `near` walks the cells nearest
//! first by the nearest any point in a cell can lie. Every answer is the
//! scan's, row for row: the index only ever narrows, and the row's own
//! point decides.

// Without the `sorted` feature the keys and covers are here but unused
// (`off.rs` stands in for the index); the distances and shapes stay.
#![cfg_attr(not(feature = "sorted"), allow(dead_code))]

use crate::error::{Error, Result};
use crate::query::{CmpOp, Expr};
use crate::value::Value;

/// The earth's radius Redis computes with (`EARTH_RADIUS_IN_METERS` in its
/// `geohash_helper.c`), in metres: a distance here is Redis's.
pub const EARTH_RADIUS_M: f64 = 6372797.560856;

/// Whether `(lon, lat)` is a point: a longitude from -180 to 180 and a
/// latitude from -90 to 90 degrees, neither NaN.
pub fn valid(lon: f64, lat: f64) -> bool {
    (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat)
}

/// A number a list gives a point or a box: an int or a float.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// The point `v` is: a `geo` field's, or `[lon, lat]` -- a list of two
/// numbers -- `None` for a null. Anything else, a longitude past 180 or a
/// latitude past 90 included, is refused rather than wrapped: a latitude
/// of 91 is a mistake, most often the two given the other way round. So is
/// a vector, the `f32`s a reader with no schema reads a list of numbers
/// into: 1.7 m off at the antimeridian, it is read again as written before
/// it gets here (`Database::exactly`). A `-0.0` is kept as `0.0`, so equal
/// points encode alike.
pub fn point_of(v: &Value) -> Result<Option<(f64, f64)>> {
    let p = match v {
        Value::Null => return Ok(None),
        Value::List(items) => match items.as_slice() {
            [a, b] => number(a).zip(number(b)),
            _ => None,
        },
        _ => None,
    };
    match p {
        Some((lon, lat)) if valid(lon, lat) => Ok(Some((lon + 0.0, lat + 0.0))),
        _ => Err(Error::Type(format!(
            "a point is [lon, lat], a longitude from -180 to 180 and a latitude from -90 to \
             90; found {}",
            crate::json::to_string(v)
        ))),
    }
}

/// The point a `geo` field holds once read, `[lon, lat]` as two floats,
/// taken as it is: it was checked as it was written.
pub fn point_in(v: &Value) -> Option<(f64, f64)> {
    match v {
        Value::List(p) => match p.as_slice() {
            [Value::Float(lon), Value::Float(lat)] => Some((*lon, *lat)),
            _ => None,
        },
        _ => None,
    }
}

// ------------------------------------------------------------ trigonometry
//
// Taylor series, each over the range a distance takes it on, evaluated by
// Horner's rule in `f64`: the same operations in the same order on every
// target, so the same bits. Held to the standard library's within a few
// units in the last place (`the_series_agree_with_libm`).

/// `sin(x)` for `|x| <= pi/4`: through x^17, the next term under 1e-19 of
/// the answer.
fn sin_series(x: f64) -> f64 {
    const S: [f64; 9] = [
        1.0,
        -0.16666666666666666,
        0.008333333333333333,
        -0.0001984126984126984,
        2.7557319223985893e-06,
        -2.505210838544172e-08,
        1.6059043836821613e-10,
        -7.647163731819816e-13,
        2.8114572543455206e-15,
    ];
    let u = x * x;
    let mut p = 0.0;
    for c in S.iter().rev() {
        p = p * u + c;
    }
    x * p
}

/// `cos(x)` for `|x| <= pi/4`: through x^18.
fn cos_series(x: f64) -> f64 {
    const C: [f64; 10] = [
        1.0,
        -0.5,
        0.041666666666666664,
        -0.001388888888888889,
        2.48015873015873e-05,
        -2.755731922398589e-07,
        2.08767569878681e-09,
        -1.1470745597729725e-11,
        4.779477332387385e-14,
        -1.5619206968586225e-16,
    ];
    let u = x * x;
    let mut p = 0.0;
    for c in C.iter().rev() {
        p = p * u + c;
    }
    p
}

use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

/// `sin(x)` for `|x| <= pi`.
pub(crate) fn sin(x: f64) -> f64 {
    let a = x.abs();
    let a = if a > FRAC_PI_2 { PI - a } else { a };
    let s = if a <= FRAC_PI_4 {
        sin_series(a)
    } else {
        cos_series(FRAC_PI_2 - a)
    };
    if x < 0.0 {
        -s
    } else {
        s
    }
}

/// `cos(x)` for `|x| <= pi`.
pub(crate) fn cos(x: f64) -> f64 {
    let a = x.abs();
    let (a, sign) = if a > FRAC_PI_2 {
        (PI - a, -1.0)
    } else {
        (a, 1.0)
    };
    let c = if a <= FRAC_PI_4 {
        cos_series(a)
    } else {
        sin_series(FRAC_PI_2 - a)
    };
    sign * c
}

/// `asin(s)` for `0 <= s <= 0.5`: through s^49, the next term under 1e-17
/// of the answer.
fn asin_series(s: f64) -> f64 {
    const A: [f64; 25] = [
        1.0,
        0.16666666666666666,
        0.075,
        0.044642857142857144,
        0.030381944444444444,
        0.022372159090909092,
        0.017352764423076924,
        0.01396484375,
        0.011551800896139705,
        0.009761609529194078,
        0.008390335809616815,
        0.0073125258735988454,
        0.006447210311889649,
        0.005740037670841924,
        0.005153309682319905,
        0.004660143486915096,
        0.004240907093679363,
        0.003880964558837669,
        0.0035692053938259347,
        0.003297059503473485,
        0.0030578216492580306,
        0.002846178401108942,
        0.00265787063820729,
        0.0024894486782468836,
        0.002338091892111975,
    ];
    let u = s * s;
    let mut p = 0.0;
    for c in A.iter().rev() {
        p = p * u + c;
    }
    s * p
}

/// `asin(y)` for `0 <= y <= 1`. Past 0.5 through `pi/2 - 2 asin(sqrt((1 -
/// y) / 2))`, whose `1 - y` is exact there.
pub(crate) fn asin(y: f64) -> f64 {
    if y <= 0.5 {
        asin_series(y)
    } else {
        FRAC_PI_2 - 2.0 * asin_series(((1.0 - y) * 0.5).sqrt())
    }
}

// ---------------------------------------------------------------- distance

/// The great-circle distance from `a` to `b`, in metres: the haversine, on
/// a sphere of [`EARTH_RADIUS_M`], as Redis computes it. The longitudes'
/// difference is taken the short way round first, so a point given as
/// `[180, lat]` and `[-180, lat]` is 0 m from itself. Symmetric to the bit.
pub fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    let mut dl = b.0 - a.0;
    if dl > 180.0 {
        dl -= 360.0;
    } else if dl < -180.0 {
        dl += 360.0;
    }
    let (p1, p2) = (a.1.to_radians(), b.1.to_radians());
    let u = sin((p2 - p1) * 0.5);
    let v = sin(dl.to_radians() * 0.5);
    let h = (u * u + cos(p1) * cos(p2) * (v * v)).min(1.0);
    2.0 * EARTH_RADIUS_M * asin(h.sqrt())
}

/// What the latitudes alone prove of `distance(a, b)` against `r`: the arc
/// along a meridian between them, less a metre and 1e-9 of it, when that is
/// past `r` -- `None` otherwise. The haversine is never below the arc
/// (`sin²(Δφ/2)` is its first term), and worked out in floats the two move
/// by less than the slack: nearly antipodal, where `asin` is steep, a fifth
/// of a metre. So a value past `r` here is past it as the distance is, and
/// a comparison of either with `r` answers alike: a scan's row ruled out
/// with no trigonometry, which most of a scan's rows are.
#[inline]
pub fn past(a: (f64, f64), b: (f64, f64), r: f64) -> Option<f64> {
    let arc = EARTH_RADIUS_M * (a.1 - b.1).abs().to_radians();
    let floor = arc - 1.0 - arc * 1e-9;
    (floor > r).then_some(floor)
}

/// A box of longitudes and latitudes: `[west, south, east, north]`,
/// GeoJSON's bounding box. A west past its east crosses the antimeridian,
/// as GeoJSON has it: `[170, -10, -170, 10]` is 20 degrees wide.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoBox {
    pub w: f64,
    pub s: f64,
    pub e: f64,
    pub n: f64,
}

impl GeoBox {
    /// Whether the point lies in the box, its edges included.
    pub fn holds(&self, (lon, lat): (f64, f64)) -> bool {
        lat >= self.s
            && lat <= self.n
            && match self.w <= self.e {
                true => lon >= self.w && lon <= self.e,
                false => lon >= self.w || lon <= self.e,
            }
    }
}

/// The box `v` is: a list of four numbers, `[west, south, east, north]`,
/// `None` for a null. A longitude past 180, a latitude past 90 or a south
/// above its north is refused.
pub fn box_of(v: &Value) -> Result<Option<GeoBox>> {
    let mut n = [0.0f64; 4];
    let whole = match v {
        Value::Null => return Ok(None),
        Value::List(items) if items.len() == 4 => {
            let mut all = true;
            for (slot, it) in n.iter_mut().zip(items) {
                match number(it) {
                    Some(x) => *slot = x,
                    None => all = false,
                }
            }
            all
        }
        _ => false,
    };
    let [w, s, e, north] = n;
    match whole && valid(w, s) && valid(e, north) && s <= north {
        true => Ok(Some(GeoBox { w, s, e, n: north })),
        false => Err(Error::Type(format!(
            "a box is [west, south, east, north] in degrees, its south not above its north; \
             found {}",
            crate::json::to_string(v)
        ))),
    }
}

/// `distance(a, b)`: the metres between two points, null where either is.
pub fn distance_fn(args: &[Value]) -> Result<Value> {
    let (Some(a), Some(b)) = (point_of(&args[0])?, point_of(&args[1])?) else {
        return Ok(Value::Null);
    };
    Ok(Value::Float(distance(a, b)))
}

/// `within(p, box)`: whether the point lies in the box, null where either
/// is.
pub fn within_fn(args: &[Value]) -> Result<Value> {
    let (Some(p), Some(b)) = (point_of(&args[0])?, box_of(&args[1])?) else {
        return Ok(Value::Null);
    };
    Ok(Value::Bool(b.holds(p)))
}

/// The two geo functions, by the lowercased names the parser gives calls.
pub const DISTANCE: &str = "distance";
pub const WITHIN: &str = "within";

/// Whether `name` calls one of them: what has a point read as written.
pub fn is_geo_call(name: &str) -> bool {
    name.eq_ignore_ascii_case(DISTANCE) || name.eq_ignore_ascii_case(WITHIN)
}

// ------------------------------------------------------------------ shapes

/// What an `and` chain's condition asks of a point: within `r` metres of a
/// centre (`distance(loc, $1) <= 500`), or in a box (`within(loc, $1)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    Circle((f64, f64), f64),
    Box(GeoBox),
}

/// `e`'s value when it is a constant: a literal or a parameter given.
fn constant<'a>(e: &'a Expr, params: &'a [Value]) -> Option<&'a Value> {
    match e {
        Expr::Lit(v) => Some(v),
        Expr::Param(i) => params.get(*i),
        _ => None,
    }
}

/// `distance(<field>, <point>)` or `distance(<point>, <field>)`: the field
/// and the point, the point constant and valid.
pub fn distance_call<'a>(e: &'a Expr, params: &[Value]) -> Option<(&'a str, (f64, f64))> {
    let Expr::Call(name, args) = e else {
        return None;
    };
    if !name.eq_ignore_ascii_case(DISTANCE) {
        return None;
    }
    let [a, b] = args.as_slice() else {
        return None;
    };
    let (field, other) = match (a, b) {
        (Expr::Field(f), o) | (o, Expr::Field(f)) => (f, o),
        _ => return None,
    };
    let p = point_of(constant(other, params)?).ok()??;
    Some((field, p))
}

/// The shape one condition asks of a field, when an index can narrow by it:
/// `distance(f, p) <= r` or `< r` -- or `r >= distance(..)`, either way
/// round -- with a radius a number from 0 up, or `within(f, box)`. A
/// constant that is no point, no box or no such radius is left to the
/// rows, which say what is wrong with it at the first one.
pub fn shape_of<'a>(e: &'a Expr, params: &[Value]) -> Option<(&'a str, Shape)> {
    match e {
        Expr::Cmp(op, a, b) => {
            let (call, r) = match op {
                CmpOp::Le | CmpOp::Lt => (a, b),
                CmpOp::Ge | CmpOp::Gt => (b, a),
                _ => return None,
            };
            let (field, p) = distance_call(call, params)?;
            let r = number(constant(r, params)?)?;
            (r >= 0.0 && r.is_finite()).then_some((field, Shape::Circle(p, r)))
        }
        Expr::Call(name, args) if name.eq_ignore_ascii_case(WITHIN) => {
            let [Expr::Field(f), b] = args.as_slice() else {
                return None;
            };
            let b = box_of(constant(b, params)?).ok()??;
            Some((f, Shape::Box(b)))
        }
        _ => None,
    }
}

/// The shapes of an `and` chain's conditions an index can narrow by. Like
/// `conjunct_ranges` it does not descend under `or` or `not`.
pub fn conjunct_shapes<'a>(f: &'a Expr, params: &[Value], out: &mut Vec<(&'a str, Shape)>) {
    match f {
        Expr::And(a, b) => {
            conjunct_shapes(a, params, out);
            conjunct_shapes(b, params, out);
        }
        other => {
            if let Some(s) = shape_of(other, params) {
                out.push(s);
            }
        }
    }
}

// -------------------------------------------------------------------- keys

/// `x` from `lo` across `span` degrees as 32 bits: `floor` of its share of
/// 2^32, held to the last value. Monotone, every step of it rounding the
/// same way, so a point within a box has its key within the box's keys --
/// which is what lets a box be cut by keys with no margin.
fn quant(x: f64, lo: f64, span: f64) -> u32 {
    let q = ((x - lo) / span * 4294967296.0).floor();
    if q >= 4294967295.0 {
        u32::MAX
    } else if q > 0.0 {
        q as u32
    } else {
        0
    }
}

fn quant_lon(lon: f64) -> u32 {
    quant(lon, -180.0, 360.0)
}

fn quant_lat(lat: f64) -> u32 {
    quant(lat, -90.0, 180.0)
}

/// The bits of `x` spread to every second place, the lowest at 0.
fn spread(x: u32) -> u64 {
    let mut v = x as u64;
    v = (v | (v << 16)) & 0x0000_FFFF_0000_FFFF;
    v = (v | (v << 8)) & 0x00FF_00FF_00FF_00FF;
    v = (v | (v << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    v = (v | (v << 2)) & 0x3333_3333_3333_3333;
    v = (v | (v << 1)) & 0x5555_5555_5555_5555;
    v
}

/// [`spread`] undone: every second bit of `v`, from bit 0.
fn squash(v: u64) -> u32 {
    let mut v = v & 0x5555_5555_5555_5555;
    v = (v | (v >> 1)) & 0x3333_3333_3333_3333;
    v = (v | (v >> 2)) & 0x0F0F_0F0F_0F0F_0F0F;
    v = (v | (v >> 4)) & 0x00FF_00FF_00FF_00FF;
    v = (v | (v >> 8)) & 0x0000_FFFF_0000_FFFF;
    v = (v | (v >> 16)) & 0x0000_0000_FFFF_FFFF;
    v as u32
}

/// A point's key: its longitude's 32 bits in the odd places and its
/// latitude's in the even ones.
pub fn key((lon, lat): (f64, f64)) -> u64 {
    (spread(quant_lon(lon)) << 1) | spread(quant_lat(lat))
}

/// A rectangle of keys' coordinates, its edges included.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
}

impl Rect {
    fn of(w: f64, e: f64, s: f64, n: f64) -> Rect {
        Rect {
            x0: quant_lon(w),
            x1: quant_lon(e),
            y0: quant_lat(s),
            y1: quant_lat(n),
        }
    }

    /// Whether the key's coordinates are inside.
    fn holds(&self, key: u64) -> bool {
        let (x, y) = (squash(key >> 1), squash(key));
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// The rectangles of keys a shape's points lie in: one, or two where it
/// crosses the antimeridian.
fn rects(shape: &Shape) -> Vec<Rect> {
    match shape {
        Shape::Box(b) if b.w <= b.e => vec![Rect::of(b.w, b.e, b.s, b.n)],
        Shape::Box(b) => vec![
            Rect::of(b.w, 180.0, b.s, b.n),
            Rect::of(-180.0, b.e, b.s, b.n),
        ],
        Shape::Circle(c, r) => circle_rects(*c, *r),
    }
}

/// The bounding box of the points within `r` metres of `c`, a little wider
/// than the cap -- 1e-7 of the angle and 1e-7 radians (0.64 m) beyond it,
/// and 1e-7 degrees on each edge: worked out in floats it must hold every
/// point the distance, worked out in floats too, puts within `r`, and a box
/// a hair wide costs nothing. A cap holding a pole spans every longitude.
fn circle_rects((lon, lat): (f64, f64), r: f64) -> Vec<Rect> {
    const PAD: f64 = 1e-7;
    let d = r / EARTH_RADIUS_M * (1.0 + PAD) + PAD;
    if d >= PI {
        return vec![Rect::of(-180.0, 180.0, -90.0, 90.0)];
    }
    let phi = lat.to_radians();
    let (lo, hi) = (phi - d, phi + d);
    let south = (lo.to_degrees() - PAD).max(-90.0);
    let north = (hi.to_degrees() + PAD).min(90.0);
    let s = sin(d) / cos(phi);
    if lo <= -FRAC_PI_2 || hi >= FRAC_PI_2 || s >= 1.0 || s.is_nan() {
        return vec![Rect::of(-180.0, 180.0, south, north)];
    }
    let dl = asin(s).to_degrees() * (1.0 + PAD) + PAD;
    let (w, e) = (lon - dl, lon + dl);
    if w < -180.0 {
        vec![
            Rect::of(w + 360.0, 180.0, south, north),
            Rect::of(-180.0, e, south, north),
        ]
    } else if e > 180.0 {
        vec![
            Rect::of(w, 180.0, south, north),
            Rect::of(-180.0, e - 360.0, south, north),
        ]
    } else {
        vec![Rect::of(w, e, south, north)]
    }
}

/// The most cells a rectangle is covered by: the finest level of the
/// quadtree at which it spans no more, each cell a range of keys read.
const MOST_CELLS: u64 = 32;

/// The ranges of keys, each from its first key to its last, of the cells
/// covering `r` at the finest level that needs no more than
/// [`MOST_CELLS`]. Keys in them outside `r` are passed over by their own
/// coordinates ([`Rect::holds`]).
fn cover(r: &Rect, out: &mut Vec<(u64, u64)>) {
    let span = |level: u32| {
        let s = 32 - level;
        let nx = (r.x1 as u64 >> s) - (r.x0 as u64 >> s) + 1;
        let ny = (r.y1 as u64 >> s) - (r.y0 as u64 >> s) + 1;
        nx * ny
    };
    let mut level = 0;
    while level < 32 && span(level + 1) <= MOST_CELLS {
        level += 1;
    }
    // Shifted as `u64`s: at level 0 by all 32 bits.
    let top = |v: u32| ((v as u64) >> (32 - level)) as u32;
    for cy in top(r.y0)..=top(r.y1) {
        for cx in top(r.x0)..=top(r.x1) {
            out.push(cell_keys(level, cx, cy));
        }
    }
}

/// The first and last key of the cell `(cx, cy)` at `level` (0 to 32): the
/// keys whose coordinates' top `level` bits are `cx` and `cy`.
fn cell_keys(level: u32, cx: u32, cy: u32) -> (u64, u64) {
    if level == 0 {
        return (0, u64::MAX);
    }
    let z = (spread(cx) << 1) | spread(cy);
    let below = 2 * (32 - level);
    if below == 0 {
        return (z, z);
    }
    let lo = z << below;
    (lo, lo | ((1u64 << below) - 1))
}

/// The cell's longitudes and latitudes in degrees: `[w, e]` and `[s, n]`.
fn cell_degrees(level: u32, cx: u32, cy: u32) -> (f64, f64, f64, f64) {
    let size = 1.0 / (1u64 << level) as f64;
    (
        cx as f64 * size * 360.0 - 180.0,
        (cx as f64 + 1.0) * size * 360.0 - 180.0,
        cy as f64 * size * 180.0 - 90.0,
        (cy as f64 + 1.0) * size * 180.0 - 90.0,
    )
}

/// The nearest a point in the cell can lie to `from` (whose latitude's
/// cosine is `cos_from`), in metres, less what the floats could take off
/// a distance: every term of the haversine at its least over the cell --
/// the latitudes' and the longitudes' gaps to the cell, and the smaller of
/// the cosines of its two edges -- so nothing in the cell is nearer.
fn cell_floor((lon, lat): (f64, f64), cos_from: f64, cell: (f64, f64, f64, f64)) -> f64 {
    let (w, e, s, n) = cell;
    let dlat = if lat < s {
        s - lat
    } else if lat > n {
        lat - n
    } else {
        0.0
    };
    // Each gap taken round to [0, 360) by a turn added: both ends are
    // within [-180, 180], so one is enough -- and `rem_euclid` was an
    // `fmod` of the browser module's own.
    let turn = |x: f64| if x < 0.0 { x + 360.0 } else { x };
    let dlon = if lon >= w && lon <= e {
        0.0
    } else {
        turn(w - lon).min(turn(lon - e)).min(180.0)
    };
    let cos_min = cos(s.to_radians()).min(cos(n.to_radians())).max(0.0);
    let u = sin(dlat.to_radians() * 0.5);
    let v = sin(dlon.to_radians() * 0.5);
    let h = (u * u + cos_from * cos_min * (v * v)).min(1.0);
    let d = 2.0 * EARTH_RADIUS_M * asin(h.sqrt());
    // A metre and 1e-7 of the distance: past what the floats move a
    // distance by -- nearly antipodal, where `asin` is steep, a tenth of a
    // metre -- and the cell's edges, an ulp of a degree off its keys'.
    d - 1.0 - d * 1e-7
}

// ------------------------------------------------------------------- index

#[cfg(feature = "sorted")]
use crate::sorted::Chunked;
#[cfg(feature = "sorted")]
use crate::value::DocId;
#[cfg(feature = "sorted")]
use std::ops::Bound;

/// The index behind `@geo`: each row's point as its key ([`key`]) beside
/// its id, ascending, in the chunks the ordered index keeps its entries in.
/// A row with no point has no entry: no condition on a point matches it,
/// and `near` passes it over. Derived data, as the other indexes are: built
/// by the first statement that reads it after an open, kept up on write,
/// never in the file.
#[cfg(feature = "sorted")]
pub struct GeoIndex {
    keys: Chunked<(u64, DocId)>,
}

/// A row's entry, `None` for a row with no point.
#[cfg(feature = "sorted")]
fn entry(id: DocId, v: Option<&Value>) -> Option<(u64, DocId)> {
    Some((key(point_in(v?)?), id))
}

/// How many rows a cell of the walk holds before its points are read
/// rather than it split in four: a split is four counts, two searches each.
#[cfg(feature = "sorted")]
const LEAF: usize = 16;

#[cfg(feature = "sorted")]
impl Default for GeoIndex {
    fn default() -> Self {
        GeoIndex::new()
    }
}

#[cfg(feature = "sorted")]
impl GeoIndex {
    pub fn new() -> GeoIndex {
        GeoIndex {
            keys: Chunked::new(None),
        }
    }

    /// Built in one pass from each row's point: sorted once and cut into
    /// chunks, as the ordered index is.
    pub fn build(rows: &mut dyn Iterator<Item = (DocId, (f64, f64))>) -> GeoIndex {
        let mut keys = Vec::new();
        for (id, p) in rows {
            keys.push((key(p), id));
        }
        crate::sorted::radix_sort(&mut keys);
        GeoIndex {
            keys: Chunked::from_sorted(keys, None),
        }
    }

    pub fn insert(&mut self, id: DocId, v: Option<&Value>) {
        if let Some(e) = entry(id, v) {
            self.keys.insert(e);
        }
    }

    /// Takes out the entry `insert` put in for the same value -- every
    /// caller reads the stored document first, as the other indexes do.
    pub fn remove(&mut self, id: DocId, v: Option<&Value>) {
        if let Some(e) = entry(id, v) {
            self.keys.remove(&e);
        }
    }

    /// The rows holding a point.
    pub fn len(&self) -> usize {
        self.keys.len
    }

    pub fn is_empty(&self) -> bool {
        self.keys.len == 0
    }

    /// Allocated bytes, estimated as the ordered index's are: chunks run
    /// between half full and full.
    pub fn memory_bytes(&self) -> usize {
        self.keys.len * std::mem::size_of::<(u64, DocId)>() * 5 / 4
    }

    fn range(&self, (lo, hi): (u64, u64)) -> impl DoubleEndedIterator<Item = &(u64, DocId)> {
        self.keys.range(
            &Bound::Included((lo, 0)),
            &Bound::Included((hi, DocId::MAX)),
        )
    }

    fn count(&self, (lo, hi): (u64, u64)) -> usize {
        match hi.checked_add(1) {
            Some(end) => self.keys.count(&(lo, 0), &(end, 0)),
            None => self.keys.count(&(lo, 0), &(u64::MAX, DocId::MAX)),
        }
    }

    /// The rows whose key lies in the shape's box, ascending -- a superset
    /// of the rows in the shape, which the filter then tests -- or `None`
    /// once more than `cap` turn up: that wide, the scan is cheaper.
    pub fn candidates(&self, shape: &Shape, cap: usize) -> Option<Vec<DocId>> {
        let mut out = Vec::new();
        let mut cells = Vec::new();
        for r in rects(shape) {
            cells.clear();
            cover(&r, &mut cells);
            for &cell in &cells {
                for &(k, id) in self.range(cell) {
                    if r.holds(k) {
                        if out.len() >= cap {
                            return None;
                        }
                        out.push(id);
                    }
                }
            }
        }
        out.sort_unstable();
        // Two boxes either side of the antimeridian can share a column of
        // keys.
        out.dedup();
        Some(out)
    }

    /// The rows nearest `from` first, `visit` handed each with its
    /// distance -- `distance(<its point>, from)`, which `point_at` reads --
    /// in the order of `(distance, id)`, until it answers `false` or the
    /// distance passes `max`.
    ///
    /// A walk of the quadtree over the keys, best first: a cell goes into
    /// the heap at the nearest any point in it can lie ([`cell_floor`]),
    /// and is split in four, each counted from the chunks, until it holds
    /// [`LEAF`] rows or fewer -- then its rows go in at their own
    /// distances. A cell's floor is never past a point of its own, and a
    /// cell ties ahead of a point, so a point leaves the heap only once
    /// every row nearer -- or as near with a lower id -- has: the order is
    /// the exact one a sort of every distance gives.
    pub fn nearest(
        &self,
        from: (f64, f64),
        max: f64,
        point_at: &mut dyn FnMut(DocId) -> Result<Option<(f64, f64)>>,
        visit: &mut dyn FnMut(DocId, f64) -> Result<bool>,
    ) -> Result<()> {
        use std::collections::BinaryHeap;
        let cos_from = cos(from.1.to_radians());
        // A cell is `(level, cx, cy, rows)`, by its place here.
        let mut cells: Vec<(u32, u32, u32, usize)> = vec![(0, 0, 0, self.keys.len)];
        let mut heap = BinaryHeap::new();
        heap.push(Near {
            at: f64::NEG_INFINITY,
            point: false,
            id: 0,
        });
        while let Some(item) = heap.pop() {
            if item.at > max {
                break;
            }
            if item.point {
                if !visit(item.id, item.at)? {
                    break;
                }
                continue;
            }
            let (level, cx, cy, n) = cells[item.id as usize];
            if n <= LEAF || level == 32 {
                for &(_, id) in self.range(cell_keys(level, cx, cy)) {
                    if let Some(p) = point_at(id)? {
                        heap.push(Near {
                            at: distance(p, from),
                            point: true,
                            id,
                        });
                    }
                }
                continue;
            }
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (x, y) = (2 * cx + dx, 2 * cy + dy);
                let n = self.count(cell_keys(level + 1, x, y));
                if n == 0 {
                    continue;
                }
                let at = cell_floor(from, cos_from, cell_degrees(level + 1, x, y));
                if at > max {
                    continue;
                }
                cells.push((level + 1, x, y, n));
                heap.push(Near {
                    at,
                    point: false,
                    id: (cells.len() - 1) as u64,
                });
            }
        }
        Ok(())
    }
}

/// An entry of [`GeoIndex::nearest`]'s heap: a cell at the nearest its
/// points can lie, or a row at its distance. Least first -- the heap is a
/// max-heap, so the order is turned round -- a cell ahead of a point at
/// the same distance, and a row ahead of one with a higher id.
#[cfg(feature = "sorted")]
struct Near {
    at: f64,
    point: bool,
    id: u64,
}

#[cfg(feature = "sorted")]
impl PartialEq for Near {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o) == std::cmp::Ordering::Equal
    }
}

#[cfg(feature = "sorted")]
impl Eq for Near {}

#[cfg(feature = "sorted")]
impl PartialOrd for Near {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}

#[cfg(feature = "sorted")]
impl Ord for Near {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        o.at.total_cmp(&self.at)
            .then(o.point.cmp(&self.point))
            .then(o.id.cmp(&self.id))
    }
}

#[cfg(not(feature = "sorted"))]
pub use crate::off::GeoIndex;

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> impl FnMut() -> f64 {
        let mut x = seed;
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Within a few units in the last place of the platform's libm, over
    /// the ranges a distance takes them on.
    #[test]
    fn the_series_agree_with_libm() {
        let mut r = rng(7);
        let ulps = |a: f64, b: f64| (a - b).abs() / b.abs().max(f64::MIN_POSITIVE) / f64::EPSILON;
        for _ in 0..200_000 {
            // A distance's half-angles and latitudes, to a few ulps.
            let x = (r() * 2.0 - 1.0) * FRAC_PI_2;
            assert!(
                ulps(sin(x), x.sin()) < 4.0 || (sin(x) - x.sin()).abs() < 1e-16,
                "sin {x}"
            );
            assert!(
                ulps(cos(x), x.cos()) < 4.0 || (cos(x) - x.cos()).abs() < 1e-16,
                "cos {x}"
            );
            // Out to pi -- a bounding box's angle -- to what `PI` itself is
            // off by.
            let x = (r() * 2.0 - 1.0) * PI;
            assert!((sin(x) - x.sin()).abs() < 1e-15, "sin {x}");
            assert!((cos(x) - x.cos()).abs() < 1e-15, "cos {x}");
            let y = r();
            assert!(ulps(asin(y), y.asin()) < 4.0, "asin {y}");
        }
        assert_eq!(asin(0.0), 0.0);
        assert_eq!(asin(1.0), FRAC_PI_2);
        assert_eq!(sin(0.0), 0.0);
        assert_eq!(cos(0.0), 1.0);
    }

    /// Redis's `GEODIST Sicily Palermo Catania`, 166274.1516 m from the
    /// cells it keeps the two in, and the same from their coordinates to
    /// within what those cells move a point by.
    #[test]
    fn a_distance_is_redis_s() {
        let palermo = (13.361389, 38.115556);
        let catania = (15.087269, 37.502669);
        let d = distance(palermo, catania);
        assert!((d - 166274.1516).abs() < 1.0, "{d}");
        assert_eq!(d, distance(catania, palermo));
        assert_eq!(distance(palermo, palermo), 0.0);
        assert_eq!(distance((180.0, 10.0), (-180.0, 10.0)), 0.0);
        // Half the way round, through the poles or along the equator.
        let half = PI * EARTH_RADIUS_M;
        assert!((distance((0.0, 0.0), (180.0, 0.0)) - half).abs() < 1e-6);
        assert!((distance((0.0, 90.0), (0.0, -90.0)) - half).abs() < 1e-6);
        assert!((distance((10.0, 90.0), (-170.0, 90.0))).abs() < 1e-6);
    }

    #[test]
    fn points_and_boxes_are_read_and_refused() {
        let pt = |a: f64, b: f64| Value::List(vec![Value::Float(a), Value::Float(b)]);
        assert_eq!(point_of(&pt(13.4, 52.5)).unwrap(), Some((13.4, 52.5)));
        assert_eq!(point_of(&Value::Null).unwrap(), None);
        for bad in [
            pt(181.0, 0.0),
            pt(0.0, 90.5),
            pt(f64::NAN, 0.0),
            Value::Int(3),
        ] {
            assert!(point_of(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            point_of(&pt(-0.0, -0.0)).unwrap().map(|p| p.0.to_bits()),
            Some(0)
        );
        let b = |v: [f64; 4]| Value::List(v.iter().map(|x| Value::Float(*x)).collect());
        let cross = box_of(&b([170.0, -10.0, -170.0, 10.0])).unwrap().unwrap();
        assert!(cross.holds((175.0, 0.0)) && cross.holds((-175.0, 0.0)));
        assert!(!cross.holds((0.0, 0.0)) && !cross.holds((175.0, 11.0)));
        assert!(box_of(&b([0.0, 10.0, 1.0, 5.0])).is_err());
    }

    #[test]
    fn keys_keep_the_order_of_their_coordinates() {
        let mut r = rng(3);
        let mut last = (0u32, 0u32, f64::NEG_INFINITY);
        let mut xs: Vec<f64> = (0..10_000).map(|_| r() * 360.0 - 180.0).collect();
        xs.extend([-180.0, 180.0, 0.0, -0.0, 179.99999999999997]);
        xs.sort_by(f64::total_cmp);
        for x in xs {
            let q = quant_lon(x);
            assert!(q >= last.0 || x == last.2, "{x}");
            last = (q, 0, x);
        }
        for _ in 0..10_000 {
            let p = (r() * 360.0 - 180.0, r() * 180.0 - 90.0);
            let k = key(p);
            assert_eq!(
                (squash(k >> 1), squash(k)),
                (quant_lon(p.0), quant_lat(p.1))
            );
        }
    }

    /// Every point a cell's keys hold is no nearer than the cell's floor.
    #[test]
    fn a_cell_s_floor_is_never_past_its_points() {
        let mut r = rng(11);
        for _ in 0..200_000 {
            let from = (r() * 360.0 - 180.0, r() * 180.0 - 90.0);
            let p = (r() * 360.0 - 180.0, r() * 180.0 - 90.0);
            let level = (r() * 33.0) as u32;
            let k = key(p);
            let (cx, cy) = match level {
                0 => (0, 0),
                l => (squash(k >> 1) >> (32 - l), squash(k) >> (32 - l)),
            };
            let floor = cell_floor(from, cos(from.1.to_radians()), cell_degrees(level, cx, cy));
            assert!(floor <= distance(p, from), "{from:?} {p:?} {level}");
            let (lo, hi) = cell_keys(level, cx, cy);
            assert!(lo <= k && k <= hi);
        }
    }

    /// The latitudes' floor never rules out a point the distance keeps, at
    /// any radius -- the poles and nearly antipodal points among them.
    #[test]
    fn the_latitude_floor_is_never_past_the_distance() {
        let mut r = rng(23);
        for i in 0..400_000 {
            let a = (r() * 360.0 - 180.0, r() * 180.0 - 90.0);
            let b = match i % 3 {
                0 => (r() * 360.0 - 180.0, r() * 180.0 - 90.0),
                // Nearly antipodal.
                1 => (
                    a.0 + 180.0 - (a.0 + 180.0 > 180.0) as u8 as f64 * 360.0,
                    -a.1 + (r() - 0.5) * 1e-6,
                ),
                _ => (
                    a.0 + (r() - 0.5) * 1e-3,
                    (a.1 + (r() - 0.5) * 1e-3).clamp(-90.0, 90.0),
                ),
            };
            let d = distance(a, b);
            for radius in [0.0, d * 0.999_999_999, d - 2.0, d - 0.5, d] {
                if let Some(floor) = past(a, b, radius) {
                    assert!(floor <= d && d > radius, "{a:?} {b:?} {radius} {floor} {d}");
                }
            }
        }
    }

    /// A circle's box holds every point within it.
    #[test]
    fn a_circle_s_box_holds_its_points() {
        let mut r = rng(5);
        for i in 0..100_000 {
            let c = (r() * 360.0 - 180.0, r() * 180.0 - 90.0);
            // Radii from a metre to half the earth, and points near the edge.
            let radius = 10f64.powf(r() * 7.3);
            let p = (r() * 360.0 - 180.0, r() * 180.0 - 90.0);
            let near = match i % 2 {
                0 => p,
                _ => {
                    let (dx, dy) = (r() - 0.5, r() - 0.5);
                    let scale = radius / 111_000.0 * 2.0;
                    (
                        (c.0 + dx * scale / c.1.to_radians().cos().max(0.01)).clamp(-180.0, 180.0),
                        (c.1 + dy * scale).clamp(-90.0, 90.0),
                    )
                }
            };
            if distance(near, c) <= radius {
                let k = key(near);
                assert!(
                    rects(&Shape::Circle(c, radius)).iter().any(|r| r.holds(k)),
                    "{c:?} {radius} {near:?}"
                );
            }
        }
    }

    #[cfg(feature = "sorted")]
    #[test]
    fn the_index_answers_as_every_point_measured() {
        let mut r = rng(17);
        let mut points = Vec::new();
        for id in 1..=3_000u64 {
            // Clustered round a few centres, with a few everywhere.
            let p = match id % 4 {
                0 => (r() * 360.0 - 180.0, r() * 180.0 - 90.0),
                1 => (13.4 + (r() - 0.5) * 0.1, 52.5 + (r() - 0.5) * 0.1),
                2 => (179.95 + (r() - 0.5) * 0.2, (r() - 0.5) * 0.2),
                _ => ((r() - 0.5) * 360.0, 89.9 + r() * 0.1),
            };
            let p = (
                ((p.0 + 180.0).rem_euclid(360.0)) - 180.0,
                p.1.clamp(-90.0, 90.0),
            );
            points.push((id, p));
        }
        let ix = GeoIndex::build(&mut points.iter().map(|(id, p)| (*id, *p)));
        let at = |id: DocId| points[id as usize - 1].1;
        for q in 0..300 {
            let from = match q % 4 {
                0 => (13.4, 52.5),
                1 => (-179.99, 0.01),
                2 => (45.0, 90.0),
                _ => (r() * 360.0 - 180.0, r() * 180.0 - 90.0),
            };
            let radius = [0.0, 50.0, 1_000.0, 30_000.0, 2e6, 2.1e7][q % 6];
            let shape = Shape::Circle(from, radius);
            let got = ix.candidates(&shape, usize::MAX).unwrap();
            for (id, p) in &points {
                if distance(*p, from) <= radius {
                    assert!(got.binary_search(id).is_ok(), "{from:?} {radius} {id}");
                }
            }
            let mut want: Vec<(f64, DocId)> = points
                .iter()
                .map(|(id, p)| (distance(*p, from), *id))
                .collect();
            want.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            let mut seen = Vec::new();
            ix.nearest(
                from,
                f64::INFINITY,
                &mut |id| Ok(Some(at(id))),
                &mut |id, d| {
                    seen.push((d, id));
                    Ok(seen.len() < 50)
                },
            )
            .unwrap();
            assert_eq!(seen, want[..50].to_vec(), "{from:?}");
        }
        let b = GeoBox {
            w: 179.9,
            s: -0.05,
            e: -179.95,
            n: 0.05,
        };
        let got = ix.candidates(&Shape::Box(b), usize::MAX).unwrap();
        for (id, p) in &points {
            if b.holds(*p) {
                assert!(got.binary_search(id).is_ok());
            }
        }
    }
}
