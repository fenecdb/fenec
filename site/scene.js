/* The hero: documents gathering into an index.

   The stars are documents. Each leaves its place and joins one of three
   neighbourhoods -- similar documents near each other -- and the edges
   between them light up as both ends arrive, which is the index being
   linked. After that a query now and then enters from a star and walks the
   edges greedily toward its nearest, which is what a search over an HNSW
   graph does.

   Plain WebGL 1, no library: three.js was measured for this scene at 100 KB
   brotli tree-shaken, against 23 KB for the whole site's JS and CSS. */

const STARS = 1500;
const ASSEMBLE = 2.6;        // seconds until the last corner lands
const QUERY_EVERY = 7.5;

/* ------------------------------------------------------------- matrices */

const mul = (a, b) => {
  const o = new Float32Array(16);
  for (let i = 0; i < 4; i++)
    for (let j = 0; j < 4; j++)
      o[j * 4 + i] = a[i] * b[j * 4] + a[4 + i] * b[j * 4 + 1] + a[8 + i] * b[j * 4 + 2] + a[12 + i] * b[j * 4 + 3];
  return o;
};
const perspective = (fov, aspect, near, far) => {
  const f = 1 / Math.tan(fov / 2), nf = 1 / (near - far);
  return new Float32Array([f / aspect, 0, 0, 0, 0, f, 0, 0, 0, 0, (far + near) * nf, -1, 0, 0, 2 * far * near * nf, 0]);
};
const translate = (x, y, z) => new Float32Array([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, x, y, z, 1]);
const scale = (s) => new Float32Array([s, 0, 0, 0, 0, s, 0, 0, 0, 0, s, 0, 0, 0, 0, 1]);
const rotY = (a) => { const c = Math.cos(a), s = Math.sin(a); return new Float32Array([c, 0, -s, 0, 0, 1, 0, 0, s, 0, c, 0, 0, 0, 0, 1]); };
const rotX = (a) => { const c = Math.cos(a), s = Math.sin(a); return new Float32Array([1, 0, 0, 0, 0, c, s, 0, 0, -s, c, 0, 0, 0, 0, 1]); };

/* ------------------------------------------------------------- shaders */

const POINT_VS = `
attribute vec3 p; attribute float s; attribute vec4 c;
uniform mat4 m; uniform float px;
varying vec4 v;
void main() { gl_Position = m * vec4(p, 1.0); gl_PointSize = s * px / gl_Position.w; v = c; }`;
const POINT_FS = `
precision mediump float; varying vec4 v;
void main() { float d = length(gl_PointCoord - 0.5); gl_FragColor = vec4(v.rgb, v.a * smoothstep(0.5, 0.0, d)); }`;
const LINE_VS = `
attribute vec3 p; attribute vec4 c; uniform mat4 m; varying vec4 v;
void main() { gl_Position = m * vec4(p, 1.0); v = c; }`;
const LINE_FS = `precision mediump float; varying vec4 v; void main() { gl_FragColor = v; }`;
function program(gl, vs, fs) {
  const p = gl.createProgram();
  for (const [type, src] of [[gl.VERTEX_SHADER, vs], [gl.FRAGMENT_SHADER, fs]]) {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    gl.attachShader(p, s);
  }
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
  const at = {}, un = {};
  for (let i = gl.getProgramParameter(p, gl.ACTIVE_ATTRIBUTES); i--;) { const a = gl.getActiveAttrib(p, i); at[a.name] = gl.getAttribLocation(p, a.name); }
  for (let i = gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS); i--;) { const u = gl.getActiveUniform(p, i); un[u.name] = gl.getUniformLocation(p, u.name); }
  return { p, at, un };
}

const hex = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16) / 255);

/* ------------------------------------------------------------- geometry */

const CLUSTERS = [
  { at: [-0.62, 0.3, 0.1], color: hex('#F4A93C') },
  { at: [0.6, 0.4, -0.25], color: hex('#4FE0C4') },
  { at: [0.02, -0.55, 0.25], color: hex('#C79BF2') },
];
const QUERY = [0.05, 0.3, 0.12];

function cloudGeometry(rand) {
  // A rough normal: the sum of three uniforms, centred.
  const g = () => (rand() + rand() + rand() - 1.5) * 0.5;
  const nodes = [], color = [];
  CLUSTERS.forEach((c) => {
    for (let n = 0; n < 95; n++) {
      nodes.push([c.at[0] + g(), c.at[1] + g() * 0.85, c.at[2] + g()]);
      color.push(c.color);
    }
  });
  // Each document linked to its three nearest, and now and then one far
  // away, as an upper layer would: one graph, crossable.
  const d2 = (a, b) => (a[0] - b[0]) ** 2 + (a[1] - b[1]) ** 2 + (a[2] - b[2]) ** 2;
  const edges = new Set();
  const link = (a, b) => { if (a !== b) edges.add(a < b ? `${a},${b}` : `${b},${a}`); };
  nodes.forEach((p, i) => {
    nodes.map((q, j) => [d2(p, q), j]).sort((a, b) => a[0] - b[0]).slice(1, 4).forEach(([, j]) => link(i, j));
    if (i % 11 === 0) link(i, Math.floor(rand() * nodes.length));
  });
  let hit = 0;
  nodes.forEach((p, i) => { if (d2(p, QUERY) < d2(nodes[hit], QUERY)) hit = i; });
  return { nodes, color, edges: [...edges].map((e) => e.split(',').map(Number)), hit };
}

/* ---------------------------------------------------------------- scene */

export function start(canvas, anchor, { still = false, onFirstFrame } = {}) {
  const gl = canvas.getContext('webgl', { antialias: true, alpha: true, premultipliedAlpha: true, powerPreference: 'low-power' });
  if (!gl) return null;

  let seed = 7;
  const rand = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);

  const points = program(gl, POINT_VS, POINT_FS);
  const lines = program(gl, LINE_VS, LINE_FS);

  const cloud = cloudGeometry(rand);
  const N = cloud.nodes.length;

  // Stars: a slab of space behind and around the cloud, thinning toward the
  // horizon the dunes cover.
  const star = new Float32Array(STARS * 3), starS = new Float32Array(STARS), starC = new Float32Array(STARS * 4);
  const tw = new Float32Array(STARS * 2);
  for (let i = 0; i < STARS; i++) {
    star[i * 3] = (rand() * 2 - 1) * 9;
    star[i * 3 + 1] = -1.2 + Math.pow(rand(), 0.8) * 6;
    star[i * 3 + 2] = -rand() * 9 + 1;
    starS[i] = 0.9 + Math.pow(rand(), 6) * 3.2;
    const warm = rand() < 0.22;
    starC.set(warm ? [1, 0.86, 0.6, 0] : [1, 0.96, 0.88, 0], i * 4);
    tw[i * 2] = rand() * 6.28; tw[i * 2 + 1] = 0.6 + rand() * 1.8;
  }

  // Where each node comes from: a star, chosen once, and when it leaves.
  const from = cloud.nodes.map(() => Math.floor(rand() * STARS));
  const delay = cloud.nodes.map(() => 0.2 + rand() * 1.4);
  const neighbours = cloud.nodes.map(() => []);
  for (const [a, b] of cloud.edges) { neighbours[a].push(b); neighbours[b].push(a); }

  const buf = () => gl.createBuffer();
  const B = { star: buf(), starS: buf(), starC: buf(), node: buf(), nodeS: buf(), nodeC: buf(), edge: buf(), edgeC: buf(), trail: buf(), trailC: buf() };
  const upload = (b, data, usage = gl.STATIC_DRAW) => { gl.bindBuffer(gl.ARRAY_BUFFER, b); gl.bufferData(gl.ARRAY_BUFFER, data, usage); };
  upload(B.star, star); upload(B.starS, starS);

  const nodeP = new Float32Array(N * 3), nodeS = new Float32Array(N), nodeC = new Float32Array(N * 4);
  const edgeP = new Float32Array(cloud.edges.length * 6), edgeC = new Float32Array(cloud.edges.length * 8);
  const progress = new Float32Array(N);

  let W = 0, H = 0, dpr = 1, proj, place = { x: 0, y: 0, s: 1 };
  const D = 7, FOV = 30 * Math.PI / 180;
  const fit = () => {
    dpr = Math.min(devicePixelRatio || 1, 1.75);
    const r = canvas.getBoundingClientRect();
    W = Math.max(1, r.width); H = Math.max(1, r.height);
    canvas.width = Math.round(W * dpr); canvas.height = Math.round(H * dpr);
    gl.viewport(0, 0, canvas.width, canvas.height);
    proj = perspective(FOV, W / H, 0.1, 40);
    // The cloud stands where the layout left room for it.
    const a = anchor.getBoundingClientRect();
    const wpp = 2 * D * Math.tan(FOV / 2) / H;
    place = {
      x: (a.left + a.width / 2 - r.left - W / 2) * wpp,
      y: -(a.top + a.height / 2 - r.top - H / 2) * wpp,
      s: a.width / 2 * wpp * 0.98,
    };
  };

  // Pointer: the cloud turns a little toward it, eased.
  let aimX = 0, aimY = 0, yaw = 0, pitch = 0;
  if (!still) {
    addEventListener('pointermove', (e) => {
      aimX = (e.clientX / innerWidth) * 2 - 1;
      aimY = (e.clientY / innerHeight) * 2 - 1;
    }, { passive: true });
  }

  // Dragged: the cloud turns with the hand, and once let go keeps the turn
  // it was given, slowing, then drifts back to the pointer's lean. A
  // vertical drag on a touch screen still scrolls the page (`pan-y`).
  let drag = null, dragYaw = 0, dragPitch = 0, spinVel = 0;
  if (!still) {
    anchor.addEventListener('pointerdown', (e) => {
      if (e.button > 0) return;
      drag = { x: e.clientX, y: e.clientY, t: performance.now() };
      spinVel = 0;
      anchor.setPointerCapture(e.pointerId);
      anchor.classList.add('grabbing');
      wake();
    });
    anchor.addEventListener('pointermove', (e) => {
      if (!drag) return;
      const now = performance.now(), dx = e.clientX - drag.x, dy = e.clientY - drag.y;
      dragYaw += dx * 0.009;
      dragPitch = Math.max(-0.7, Math.min(0.7, dragPitch + dy * 0.006));
      // Radians a frame, from the last move's pace.
      spinVel = (dx * 0.009) / Math.max(8, now - drag.t) * 16;
      drag = { x: e.clientX, y: e.clientY, t: now };
    });
    const release = () => { drag = null; anchor.classList.remove('grabbing'); };
    anchor.addEventListener('pointerup', release);
    anchor.addEventListener('pointercancel', release);
    anchor.classList.add('turnable');
  }

  // A query: from a star into the graph, then greedily along edges to the
  // node nearest the target, a hop at a time.
  let query = null, nextQuery = ASSEMBLE + 1.2;
  const target = cloud.nodes[cloud.hit];
  const dist = (i) => (cloud.nodes[i][0] - target[0]) ** 2 + (cloud.nodes[i][1] - target[1]) ** 2 + (cloud.nodes[i][2] - target[2]) ** 2;
  const launch = (t) => {
    let at = Math.floor(rand() * N);
    // Enter far from the answer, so the walk has somewhere to go.
    for (let k = 0; k < 6; k++) { const c = Math.floor(rand() * N); if (dist(c) > dist(at)) at = c; }
    const path = [at];
    for (let guard = 0; guard < 40; guard++) {
      let best = at;
      for (const n of neighbours[at]) if (dist(n) < dist(best)) best = n;
      if (best === at) break;
      path.push(at = best);
    }
    if (at !== cloud.hit) path.push(cloud.hit);
    // Enter from a star beside the cloud, never from across the headline.
    let from = Math.floor(rand() * STARS);
    for (let k = 0; k < 40 && Math.abs(star[from * 3] - place.x) > place.s * 2.2; k++) from = Math.floor(rand() * STARS);
    query = { t0: t, path, hop: 0.2, from };
  };

  const ease = (x) => (x <= 0 ? 0 : x >= 1 ? 1 : 1 - Math.pow(1 - x, 3));
  let t0 = performance.now(), running = false, first = true, awake = true;

  const draw = (now) => {
    const t = still ? 99 : (now - t0) / 1000;
    yaw += ((still ? 0 : aimX * 0.32 + Math.sin(t * 0.31) * 0.05) - yaw) * 0.045;
    pitch += ((still ? 0 : aimY * 0.14) - pitch) * 0.045;

    // A slow turn, so the cloud reads as three-dimensional, plus the pointer.
    const spin = still ? 0.5 : t * 0.09;
    if (!drag) {
      dragYaw += spinVel;
      spinVel *= 0.95;
      dragPitch *= 0.97;
    }
    const model = mul(translate(place.x, place.y, 0), mul(scale(place.s), mul(rotY(yaw + spin + dragYaw), rotX(pitch * 0.8 - 0.04 + dragPitch))));
    const view = translate(0, 0, -D);
    const vp = mul(proj, view);
    const cloudM = mul(vp, model);
    const starM = mul(vp, rotY(yaw * 0.12));

    // Each node's flight: a star's place in the cloud's space to its place.
    const inv = 1 / place.s;
    for (let i = 0; i < N; i++) {
      const k = ease((t - delay[i]) / 1.15);
      progress[i] = k;
      const s = from[i] * 3;
      const sx = (star[s] - place.x) * inv, sy = (star[s + 1] - place.y) * inv, sz = star[s + 2] * inv;
      const p = cloud.nodes[i];
      nodeP[i * 3] = sx + (p[0] - sx) * k;
      nodeP[i * 3 + 1] = sy + (p[1] - sy) * k + Math.sin(k * Math.PI) * 0.12;
      nodeP[i * 3 + 2] = sz + (p[2] - sz) * k;
      const [r, g, b] = cloud.color[i];
      nodeS[i] = 6.4;
      nodeC.set([r + (1 - r) * (1 - k), g + (1 - g) * (1 - k), b + (1 - b) * (1 - k), 0.35 + 0.6 * k], i * 4);
    }
    cloud.edges.forEach(([a, b], e) => {
      edgeP.set(nodeP.subarray(a * 3, a * 3 + 3), e * 6);
      edgeP.set(nodeP.subarray(b * 3, b * 3 + 3), e * 6 + 3);
      const k = Math.min(progress[a], progress[b]);
      const al = k * k * 0.34;
      edgeC.set([1, 0.8, 0.5, al, 1, 0.8, 0.5, al], e * 8);
    });

    // Twinkle, and the stars a node left dim while it is away.
    for (let i = 0; i < STARS; i++) {
      starC[i * 4 + 3] = (0.42 + 0.38 * Math.sin(t * tw[i * 2 + 1] + tw[i * 2])) * (still ? 0.9 : 1);
    }
    for (let i = 0; i < N; i++) starC[from[i] * 4 + 3] *= 1 - Math.min(1, progress[i] * 3) * 0.7;

    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);

    const attr = (prog, name, b, size) => {
      const loc = prog.at[name];
      gl.bindBuffer(gl.ARRAY_BUFFER, b);
      gl.enableVertexAttribArray(loc);
      gl.vertexAttribPointer(loc, size, gl.FLOAT, false, 0, 0);
    };
    const off = (prog) => { for (const k in prog.at) gl.disableVertexAttribArray(prog.at[k]); };

    gl.enable(gl.BLEND);
    gl.disable(gl.DEPTH_TEST);

    // Stars.
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE);
    gl.useProgram(points.p);
    upload(B.starC, starC, gl.DYNAMIC_DRAW);
    attr(points, 'p', B.star, 3); attr(points, 's', B.starS, 1); attr(points, 'c', B.starC, 4);
    gl.uniformMatrix4fv(points.un.m, false, starM);
    gl.uniform1f(points.un.px, dpr * 6);
    gl.drawArrays(gl.POINTS, 0, STARS);
    off(points);

    // The graph: edges, then nodes.
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE);
    gl.useProgram(lines.p);
    upload(B.edge, edgeP, gl.DYNAMIC_DRAW); upload(B.edgeC, edgeC, gl.DYNAMIC_DRAW);
    attr(lines, 'p', B.edge, 3); attr(lines, 'c', B.edgeC, 4);
    gl.uniformMatrix4fv(lines.un.m, false, cloudM);
    gl.drawArrays(gl.LINES, 0, cloud.edges.length * 2);

    // A query's walk: the hops so far, bright, fading behind it.
    let walkHead = -1;
    if (!still && t > nextQuery && !query) launch(t);
    if (query) {
      const steps = (t - query.t0) / query.hop;
      const walk = steps - 1;   // the first hop is the way in from the star
      const done = Math.max(0, Math.min(Math.floor(walk), query.path.length - 1));
      const tp = [], tc = [];
      // In from the star it started at, then along the edges.
      {
        const s = query.from * 3, inv = 1 / place.s, a = query.path[0];
        const k = Math.min(1, steps);
        const sx = (star[s] - place.x) * inv, sy = (star[s + 1] - place.y) * inv, sz = star[s + 2] * inv;
        const ex = nodeP[a * 3], ey = nodeP[a * 3 + 1], ez = nodeP[a * 3 + 2];
        const age = Math.max(0, 1 - steps / 14);
        tp.push(sx, sy, sz, sx + (ex - sx) * k, sy + (ey - sy) * k, sz + (ez - sz) * k);
        tc.push(0.31, 0.88, 0.77, age * 0.6, 0.31, 0.88, 0.77, age);
      }
      for (let h = 0; h < done; h++) {
        const a = query.path[h], b = query.path[h + 1];
        const age = Math.max(0, 1 - (walk - h) / 14);
        tp.push(...nodeP.subarray(a * 3, a * 3 + 3), ...nodeP.subarray(b * 3, b * 3 + 3));
        tc.push(0.12, 0.75, 0.64, age, 0.12, 0.75, 0.64, age);
      }
      walkHead = walk < 0 ? -1 : query.path[done];
      if (tp.length) {
        gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
        upload(B.trail, new Float32Array(tp), gl.DYNAMIC_DRAW); upload(B.trailC, new Float32Array(tc), gl.DYNAMIC_DRAW);
        attr(lines, 'p', B.trail, 3); attr(lines, 'c', B.trailC, 4);
        gl.drawArrays(gl.LINES, 0, tp.length / 3);
      }
      if (steps > query.path.length + 14) { query = null; nextQuery = t + QUERY_EVERY; }
    }
    off(lines);

    // Nodes are laid over rather than added: teal added to amber is white,
    // and the walk would vanish into the cloud.
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.useProgram(points.p);
    // The nodes the walk has measured stay lit a while behind it.
    if (query) {
      const walk = (t - query.t0) / query.hop - 1;
      query.path.forEach((n, h) => {
        if (h > walk) return;
        const age = Math.max(0, 1 - (walk - h) / 14);
        nodeS[n] = Math.max(nodeS[n], 6 + 10 * age);
        nodeC.set([0.2, 0.92, 0.78, Math.max(nodeC[n * 4 + 3], age)], n * 4);
      });
    }
    if (walkHead >= 0) {
      nodeS[walkHead] = 22;
      nodeC.set([0.62, 1, 0.93, 1], walkHead * 4);
    }
    upload(B.node, nodeP, gl.DYNAMIC_DRAW); upload(B.nodeS, nodeS, gl.DYNAMIC_DRAW); upload(B.nodeC, nodeC, gl.DYNAMIC_DRAW);
    attr(points, 'p', B.node, 3); attr(points, 's', B.nodeS, 1); attr(points, 'c', B.nodeC, 4);
    gl.uniformMatrix4fv(points.un.m, false, cloudM);
    gl.uniform1f(points.un.px, dpr * 6);
    gl.drawArrays(gl.POINTS, 0, N);
    off(points);

    if (first) { first = false; onFirstFrame?.(); }
    running = !still && awake && !document.hidden;
    if (running) requestAnimationFrame(draw);
  };

  const wake = () => { if (!running && !still) { running = true; requestAnimationFrame(draw); } };
  fit();
  requestAnimationFrame(draw);
  addEventListener('resize', () => { fit(); if (still) requestAnimationFrame(draw); else wake(); }, { passive: true });
  addEventListener('visibilitychange', wake);
  new IntersectionObserver((e) => { awake = e.some((x) => x.isIntersecting); if (awake) wake(); }).observe(canvas);

  return {
    // The hero's query has finished typing: run it through the graph now.
    ask() {
      const t = (performance.now() - t0) / 1000;
      if (still || query) return;
      if (t < ASSEMBLE + 0.3) nextQuery = ASSEMBLE + 0.3; else launch(t);
    },
  };
}
