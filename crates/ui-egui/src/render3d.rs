//! A small software 3D renderer that draws into an [`egui::Mesh`].
//!
//! - Geometry: [`Mesh3`] with generators for icospheres, rounded boxes, tori (full or arcs),
//!   lathed profiles (cylinders, cones, pucks, bins), bevelled extrusions of 2D outlines
//!   (shields, gears, sectors) and faceted gems.
//! - Camera: perspective, looking at the origin; world units are about one per object radius.
//! - Light: per-vertex, a hemisphere ambient plus key, fill and back (rim) lights, Blinn-Phong
//!   specular, a Fresnel rim tinted per material and emissive glow; filmic tone mapping.
//! - Visibility: back faces culled for opaque materials, every triangle sorted back to front
//!   (painter's algorithm, so translucent glass blends correctly and nothing z-fights).
//! - Edges: silhouette edges get a one-pixel feather strip (the way egui feathers its own
//!   shapes), so outlines are smooth on the GPU without multisampling.
//! - Sprites: glows (additive), soft dots, four-point glints and contact shadows, sorted with
//!   the triangles.
//!
//! [`rasterize`] turns an egui `Mesh` into pixels on the CPU (supersampled), for the app icon
//! generator and for tests that look at coverage without a GPU.

use egui::epaint::{Vertex, WHITE_UV};
use egui::{Color32, Mesh, Pos2, Rect, Rgba};
use std::collections::HashMap;
use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::ops::{Add, Mul, Neg, Sub};

// ---------------------------------------------------------------- math

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct V3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

pub const fn v3(x: f32, y: f32, z: f32) -> V3 {
    V3 { x, y, z }
}

impl V3 {
    pub const ZERO: V3 = v3(0.0, 0.0, 0.0);
    pub const Y: V3 = v3(0.0, 1.0, 0.0);
    pub fn dot(self, o: V3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    pub fn cross(self, o: V3) -> V3 {
        v3(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }
    pub fn len(self) -> f32 {
        self.dot(self).sqrt()
    }
    pub fn norm(self) -> V3 {
        let l = self.len();
        if l > 1e-12 { self * (1.0 / l) } else { V3::Y }
    }
    pub fn lerp(self, o: V3, t: f32) -> V3 {
        self + (o - self) * t
    }
}

impl Add for V3 {
    type Output = V3;
    fn add(self, o: V3) -> V3 {
        v3(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}
impl Sub for V3 {
    type Output = V3;
    fn sub(self, o: V3) -> V3 {
        v3(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}
impl Mul<f32> for V3 {
    type Output = V3;
    fn mul(self, s: f32) -> V3 {
        v3(self.x * s, self.y * s, self.z * s)
    }
}
impl Neg for V3 {
    type Output = V3;
    fn neg(self) -> V3 {
        v3(-self.x, -self.y, -self.z)
    }
}

/// Row-major 3x3 matrix.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct M3(pub [[f32; 3]; 3]);

impl M3 {
    pub const I: M3 = M3([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
    pub fn rot_x(a: f32) -> M3 {
        let (s, c) = a.sin_cos();
        M3([[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]])
    }
    pub fn rot_y(a: f32) -> M3 {
        let (s, c) = a.sin_cos();
        M3([[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]])
    }
    pub fn rot_z(a: f32) -> M3 {
        let (s, c) = a.sin_cos();
        M3([[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]])
    }
    pub fn scale(s: V3) -> M3 {
        M3([[s.x, 0.0, 0.0], [0.0, s.y, 0.0], [0.0, 0.0, s.z]])
    }
    pub fn apply(&self, v: V3) -> V3 {
        let m = &self.0;
        v3(
            m[0][0] * v.x + m[0][1] * v.y + m[0][2] * v.z,
            m[1][0] * v.x + m[1][1] * v.y + m[1][2] * v.z,
            m[2][0] * v.x + m[2][1] * v.y + m[2][2] * v.z,
        )
    }
    /// `self * o` (apply `o` first).
    pub fn mul(&self, o: &M3) -> M3 {
        let mut r = [[0.0; 3]; 3];
        for (i, row) in r.iter_mut().enumerate() {
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = (0..3).map(|k| self.0[i][k] * o.0[k][j]).sum();
            }
        }
        M3(r)
    }
    /// Inverse transpose (for normals); identity when singular.
    pub fn normal_matrix(&self) -> M3 {
        let m = &self.0;
        let c = |a: usize, b: usize, c: usize, d: usize| m[a][b] * m[c][d] - m[a][d] * m[c][b];
        let cof = [
            [c(1, 1, 2, 2), -c(1, 0, 2, 2), c(1, 0, 2, 1)],
            [-c(0, 1, 2, 2), c(0, 0, 2, 2), -c(0, 0, 2, 1)],
            [c(0, 1, 1, 2), -c(0, 0, 1, 2), c(0, 0, 1, 1)],
        ];
        let det = m[0][0] * cof[0][0] + m[0][1] * cof[0][1] + m[0][2] * cof[0][2];
        if det.abs() < 1e-12 {
            return M3::I;
        }
        // inverse = adj / det = cof^T / det, so (inverse)^T = cof / det.
        let s = 1.0 / det;
        M3(cof.map(|r| r.map(|v| v * s)))
    }
}

/// An affine transform `p -> m * p + t`, built by chaining (each call applies after the
/// previous ones): `Xf::scale(0.5).rot_y(a).at(v3(0.0, 1.0, 0.0))`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Xf {
    pub m: M3,
    pub t: V3,
}

impl Default for Xf {
    fn default() -> Self {
        Xf::ID
    }
}

impl Xf {
    pub const ID: Xf = Xf {
        m: M3::I,
        t: V3::ZERO,
    };
    pub fn scale(s: f32) -> Xf {
        Xf::ID.then_m(M3::scale(v3(s, s, s)))
    }
    pub fn scale3(s: V3) -> Xf {
        Xf::ID.then_m(M3::scale(s))
    }
    fn then_m(self, m: M3) -> Xf {
        Xf {
            m: m.mul(&self.m),
            t: m.apply(self.t),
        }
    }
    pub fn rot_x(self, a: f32) -> Xf {
        self.then_m(M3::rot_x(a))
    }
    pub fn rot_y(self, a: f32) -> Xf {
        self.then_m(M3::rot_y(a))
    }
    pub fn rot_z(self, a: f32) -> Xf {
        self.then_m(M3::rot_z(a))
    }
    pub fn scaled(self, s: f32) -> Xf {
        self.then_m(M3::scale(v3(s, s, s)))
    }
    pub fn scaled3(self, s: V3) -> Xf {
        self.then_m(M3::scale(s))
    }
    pub fn at(self, d: V3) -> Xf {
        Xf {
            m: self.m,
            t: self.t + d,
        }
    }
    /// `self` then `o`.
    pub fn then(self, o: Xf) -> Xf {
        Xf {
            m: o.m.mul(&self.m),
            t: o.m.apply(self.t) + o.t,
        }
    }
    pub fn apply(&self, p: V3) -> V3 {
        self.m.apply(p) + self.t
    }
}

// ---------------------------------------------------------------- colour

/// Linear RGB.
pub type Lin = [f32; 3];

fn lin_from_u8(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// An sRGB colour as linear RGB.
pub fn lin(c: Color32) -> Lin {
    [lin_from_u8(c.r()), lin_from_u8(c.g()), lin_from_u8(c.b())]
}

pub fn mix(a: Lin, b: Lin, t: f32) -> Lin {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

pub fn scale(a: Lin, s: f32) -> Lin {
    [a[0] * s, a[1] * s, a[2] * s]
}

pub const WHITE: Lin = [1.0, 1.0, 1.0];
pub const BLACK: Lin = [0.0, 0.0, 0.0];

/// Filmic curve (Narkowicz ACES fit): soft highlights instead of clipping.
fn tone(x: f32) -> f32 {
    let x = x.max(0.0);
    ((x * (2.51 * x + 0.03)) / (x * (2.43 * x + 0.59) + 0.14)).clamp(0.0, 1.0)
}

/// Linear colour and alpha to an egui colour (premultiplied, gamma).
fn out_color(c: Lin, a: f32) -> Color32 {
    let a = a.clamp(0.0, 1.0);
    Color32::from(Rgba::from_rgba_unmultiplied(
        tone(c[0]),
        tone(c[1]),
        tone(c[2]),
        a,
    ))
}

/// Additive colour (alpha 0 in premultiplied space adds light to what is behind).
pub fn additive(c: Lin, k: f32) -> Color32 {
    let g = |v: f32| Rgba::from_rgb(v * k, 0.0, 0.0).to_srgba_unmultiplied()[0];
    Color32::from_rgba_premultiplied(g(c[0]), g(c[1]), g(c[2]), 0)
}

/// How a surface reacts to light.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    /// Colour at the top of the mesh (tint 1).
    pub top: Lin,
    /// Colour at the bottom (tint 0).
    pub bottom: Lin,
    /// 1 opaque, below 1 glass (back faces are drawn and blended).
    pub alpha: f32,
    /// How much glass becomes opaque at grazing angles.
    pub fresnel_alpha: f32,
    /// Specular strength and exponent.
    pub spec: f32,
    pub shine: f32,
    /// Fresnel rim light, tinted.
    pub rim: f32,
    pub rim_col: Lin,
    pub emissive: Lin,
    /// Diffuse strength (lower for metals and glass).
    pub diffuse: f32,
}

impl Material {
    /// Glossy plastic in a two-colour gradient.
    pub fn gloss(top: Lin, bottom: Lin) -> Material {
        Material {
            top,
            bottom,
            alpha: 1.0,
            fresnel_alpha: 0.0,
            spec: 0.55,
            shine: 48.0,
            rim: 0.55,
            rim_col: mix(top, WHITE, 0.4),
            emissive: BLACK,
            diffuse: 1.0,
        }
    }
    /// Satin: softer, wider highlight.
    pub fn satin(top: Lin, bottom: Lin) -> Material {
        Material {
            spec: 0.3,
            shine: 18.0,
            ..Material::gloss(top, bottom)
        }
    }
    /// Polished metal: dim diffuse, strong tight specular, bright rim.
    pub fn metal(top: Lin, bottom: Lin) -> Material {
        Material {
            diffuse: 0.55,
            spec: 1.1,
            shine: 70.0,
            rim: 0.8,
            ..Material::gloss(top, bottom)
        }
    }
    /// Glass: mostly transparent, opaque at the edges, sharp highlight.
    pub fn glass(tint: Lin, alpha: f32) -> Material {
        Material {
            top: tint,
            bottom: tint,
            alpha,
            fresnel_alpha: 0.75,
            spec: 1.2,
            shine: 90.0,
            rim: 0.9,
            rim_col: mix(tint, WHITE, 0.6),
            emissive: BLACK,
            diffuse: 0.45,
        }
    }
    pub fn with_emissive(mut self, e: Lin) -> Material {
        self.emissive = e;
        self
    }
    pub fn with_rim(mut self, k: f32, col: Lin) -> Material {
        self.rim = k;
        self.rim_col = col;
        self
    }
}

/// A directional light: direction *towards* the light, colour times intensity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    pub dir: V3,
    pub col: Lin,
    /// Contributes to the specular highlight.
    pub spec: bool,
}

/// Key, fill and back light plus a sky/ground ambient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rig {
    pub lights: [Light; 3],
    pub sky: Lin,
    pub ground: Lin,
}

impl Rig {
    /// Studio lighting: warm key from the upper left front, cool fill from the right, a back
    /// light above and behind that outlines the silhouette, ambient tinted by `env`.
    pub fn studio(env: Lin) -> Rig {
        Rig {
            lights: [
                Light {
                    dir: v3(-0.55, 0.75, 0.62).norm(),
                    col: [1.05, 1.0, 0.95],
                    spec: true,
                },
                Light {
                    dir: v3(0.85, 0.05, 0.45).norm(),
                    col: scale(mix([0.6, 0.75, 1.0], env, 0.4), 0.38),
                    spec: false,
                },
                Light {
                    dir: v3(0.25, 0.55, -0.85).norm(),
                    col: scale(mix(WHITE, env, 0.5), 0.75),
                    spec: true,
                },
            ],
            sky: scale(mix([0.55, 0.62, 0.75], env, 0.35), 0.32),
            ground: scale(mix([0.05, 0.05, 0.07], env, 0.3), 0.18),
        }
    }
}

/// Perspective camera looking at `target`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub eye: V3,
    pub target: V3,
    /// Vertical field of view in radians.
    pub fov: f32,
}

impl Camera {
    /// From the front, a little above, `dist` away.
    pub fn front(dist: f32, elevation: f32) -> Camera {
        Camera {
            eye: v3(0.0, dist * elevation.sin(), dist * elevation.cos()),
            target: V3::ZERO,
            fov: 30f32.to_radians(),
        }
    }
}

struct View {
    eye: V3,
    right: V3,
    up: V3,
    fwd: V3,
    focal: f32,
    center: Pos2,
}

impl View {
    fn new(cam: &Camera, rect: Rect) -> View {
        let fwd = (cam.target - cam.eye).norm();
        let right = fwd.cross(V3::Y).norm();
        let up = right.cross(fwd);
        let half = rect.width().min(rect.height()) * 0.5;
        View {
            eye: cam.eye,
            right,
            up,
            fwd,
            focal: half / (cam.fov * 0.5).tan(),
            center: rect.center(),
        }
    }
    /// Screen position and depth.
    fn project(&self, p: V3) -> (Pos2, f32) {
        let d = p - self.eye;
        let z = d.dot(self.fwd).max(1e-3);
        let k = self.focal / z;
        (
            Pos2::new(
                self.center.x + d.dot(self.right) * k,
                self.center.y - d.dot(self.up) * k,
            ),
            z,
        )
    }
}

// ---------------------------------------------------------------- geometry

const NONE: u32 = u32::MAX;

/// A triangle mesh with per-vertex normals and a gradient coordinate (`tint`, 0 at the bottom
/// and 1 at the top). Call [`Mesh3::finish`] after building (generators do).
#[derive(Clone, Debug, Default)]
pub struct Mesh3 {
    pub pos: Vec<V3>,
    pub nrm: Vec<V3>,
    pub tint: Vec<f32>,
    pub tris: Vec<[u32; 3]>,
    /// (a, b, front triangle, other triangle or NONE): every edge once, for silhouettes.
    edges: Vec<[u32; 4]>,
    /// Centre of the bounding box (the depth an opaque object is sorted by).
    pub center: V3,
}

impl Mesh3 {
    pub fn vertex(&mut self, p: V3, n: V3) -> u32 {
        self.pos.push(p);
        self.nrm.push(n.norm());
        (self.pos.len() - 1) as u32
    }

    pub fn tri(&mut self, a: u32, b: u32, c: u32) {
        self.tris.push([a, b, c]);
    }

    /// Append another mesh (keeps its tint values).
    pub fn merge(&mut self, o: &Mesh3) {
        let base = self.pos.len() as u32;
        self.pos.extend_from_slice(&o.pos);
        self.nrm.extend_from_slice(&o.nrm);
        self.tint.resize(self.pos.len() - o.pos.len(), 0.5);
        if o.tint.len() == o.pos.len() {
            self.tint.extend_from_slice(&o.tint);
        } else {
            self.tint.resize(self.pos.len(), 0.5);
        }
        self.tris.extend(
            o.tris
                .iter()
                .map(|t| [t[0] + base, t[1] + base, t[2] + base]),
        );
        self.edges.clear();
    }

    /// Bake a transform into the vertices.
    pub fn transformed(mut self, xf: Xf) -> Mesh3 {
        let nm = xf.m.normal_matrix();
        for p in &mut self.pos {
            *p = xf.apply(*p);
        }
        for n in &mut self.nrm {
            *n = nm.apply(*n).norm();
        }
        self.finish()
    }

    /// One normal per face (faceted look): every triangle gets its own three vertices.
    pub fn faceted(self) -> Mesh3 {
        let mut m = Mesh3::default();
        for t in &self.tris {
            let [a, b, c] = t.map(|i| self.pos[i as usize]);
            let n = (b - a).cross(c - a).norm();
            let i = m.vertex(a, n);
            m.vertex(b, n);
            m.vertex(c, n);
            m.tri(i, i + 1, i + 2);
            for &v in t {
                m.tint
                    .push(self.tint.get(v as usize).copied().unwrap_or(0.5));
            }
        }
        m.finish()
    }

    /// Fix triangle winding to agree with the vertex normals, drop degenerate triangles, fill
    /// the gradient coordinate when missing and find the edges used for silhouettes.
    pub fn finish(mut self) -> Mesh3 {
        self.tris.retain(|t| {
            let [a, b, c] = t.map(|i| self.pos[i as usize]);
            (b - a).cross(c - a).len() > 1e-9
        });
        for t in &mut self.tris {
            let [a, b, c] = t.map(|i| self.pos[i as usize]);
            let g = (b - a).cross(c - a);
            let n = self.nrm[t[0] as usize] + self.nrm[t[1] as usize] + self.nrm[t[2] as usize];
            if g.dot(n) < 0.0 {
                t.swap(1, 2);
            }
        }
        if self.tint.len() != self.pos.len() {
            let (lo, hi) = self
                .pos
                .iter()
                .fold((f32::MAX, f32::MIN), |(l, h), p| (l.min(p.y), h.max(p.y)));
            let span = (hi - lo).max(1e-6);
            self.tint = self.pos.iter().map(|p| (p.y - lo) / span).collect();
        }
        // Weld equal positions so seams between duplicated vertices are not silhouettes.
        let q = |p: V3| {
            (
                (p.x * 1e4).round() as i64,
                (p.y * 1e4).round() as i64,
                (p.z * 1e4).round() as i64,
            )
        };
        let mut weld: HashMap<(i64, i64, i64), u32> = HashMap::new();
        let canon: Vec<u32> = self
            .pos
            .iter()
            .enumerate()
            .map(|(i, p)| *weld.entry(q(*p)).or_insert(i as u32))
            .collect();
        let mut map: HashMap<(u32, u32), usize> = HashMap::new();
        let mut edges: Vec<[u32; 4]> = Vec::new();
        for (ti, t) in self.tris.iter().enumerate() {
            for k in 0..3 {
                let (a, b) = (t[k], t[(k + 1) % 3]);
                let (ca, cb) = (canon[a as usize], canon[b as usize]);
                let key = (ca.min(cb), ca.max(cb));
                match map.get(&key) {
                    Some(&e) if edges[e][3] == NONE => edges[e][3] = ti as u32,
                    Some(_) => {}
                    None => {
                        map.insert(key, edges.len());
                        edges.push([a, b, ti as u32, NONE]);
                    }
                }
            }
        }
        self.edges = edges;
        let (lo, hi) = self.pos.iter().fold(
            (
                v3(f32::MAX, f32::MAX, f32::MAX),
                v3(f32::MIN, f32::MIN, f32::MIN),
            ),
            |(l, h), p| {
                (
                    v3(l.x.min(p.x), l.y.min(p.y), l.z.min(p.z)),
                    v3(h.x.max(p.x), h.y.max(p.y), h.z.max(p.z)),
                )
            },
        );
        self.center = if self.pos.is_empty() {
            V3::ZERO
        } else {
            (lo + hi) * 0.5
        };
        self
    }

    /// Unit icosphere subdivided `sub` times (smooth).
    pub fn icosphere(sub: u32) -> Mesh3 {
        let t = (1.0 + 5f32.sqrt()) / 2.0;
        let mut p: Vec<V3> = [
            (-1.0, t, 0.0),
            (1.0, t, 0.0),
            (-1.0, -t, 0.0),
            (1.0, -t, 0.0),
            (0.0, -1.0, t),
            (0.0, 1.0, t),
            (0.0, -1.0, -t),
            (0.0, 1.0, -t),
            (t, 0.0, -1.0),
            (t, 0.0, 1.0),
            (-t, 0.0, -1.0),
            (-t, 0.0, 1.0),
        ]
        .iter()
        .map(|&(x, y, z)| v3(x, y, z).norm())
        .collect();
        let mut f: Vec<[u32; 3]> = vec![
            [0, 11, 5],
            [0, 5, 1],
            [0, 1, 7],
            [0, 7, 10],
            [0, 10, 11],
            [1, 5, 9],
            [5, 11, 4],
            [11, 10, 2],
            [10, 7, 6],
            [7, 1, 8],
            [3, 9, 4],
            [3, 4, 2],
            [3, 2, 6],
            [3, 6, 8],
            [3, 8, 9],
            [4, 9, 5],
            [2, 4, 11],
            [6, 2, 10],
            [8, 6, 7],
            [9, 8, 1],
        ];
        for _ in 0..sub {
            let mut mid: HashMap<(u32, u32), u32> = HashMap::new();
            let mut nf = Vec::with_capacity(f.len() * 4);
            let mut m = |a: u32, b: u32, p: &mut Vec<V3>| {
                *mid.entry((a.min(b), a.max(b))).or_insert_with(|| {
                    p.push((p[a as usize] + p[b as usize]).norm());
                    (p.len() - 1) as u32
                })
            };
            for [a, b, c] in f {
                let ab = m(a, b, &mut p);
                let bc = m(b, c, &mut p);
                let ca = m(c, a, &mut p);
                nf.extend([[a, ab, ca], [b, bc, ab], [c, ca, bc], [ab, bc, ca]]);
            }
            f = nf;
        }
        Mesh3 {
            nrm: p.clone(),
            pos: p,
            tint: Vec::new(),
            tris: f,
            edges: Vec::new(),
            center: V3::ZERO,
        }
        .finish()
    }

    /// Box of half sizes `h` with edges rounded by radius `r` (`n` grid steps per face).
    pub fn rounded_box(h: V3, r: f32, n: usize) -> Mesh3 {
        let r = r.min(h.x).min(h.y).min(h.z).max(0.0);
        let inner = v3(h.x - r, h.y - r, h.z - r);
        let mut m = Mesh3::default();
        // Grid coordinates: dense near the rounded edges.
        let n = n.max(2) | 1;
        let ts: Vec<f32> = (0..=n)
            .map(|i| {
                let u = i as f32 / n as f32 * 2.0 - 1.0;
                u.signum() * (1.0 - (1.0 - u.abs()).powf(1.6))
            })
            .collect();
        let faces: [(V3, V3, V3); 6] = [
            (v3(1.0, 0.0, 0.0), v3(0.0, 1.0, 0.0), v3(0.0, 0.0, 1.0)),
            (v3(-1.0, 0.0, 0.0), v3(0.0, 0.0, 1.0), v3(0.0, 1.0, 0.0)),
            (v3(0.0, 1.0, 0.0), v3(0.0, 0.0, 1.0), v3(1.0, 0.0, 0.0)),
            (v3(0.0, -1.0, 0.0), v3(1.0, 0.0, 0.0), v3(0.0, 0.0, 1.0)),
            (v3(0.0, 0.0, 1.0), v3(1.0, 0.0, 0.0), v3(0.0, 1.0, 0.0)),
            (v3(0.0, 0.0, -1.0), v3(0.0, 1.0, 0.0), v3(1.0, 0.0, 0.0)),
        ];
        for (nrm, u, v) in faces {
            let base = m.pos.len() as u32;
            for &tv in &ts {
                for &tu in &ts {
                    let c = nrm + u * tu + v * tv;
                    let p = v3(c.x * h.x, c.y * h.y, c.z * h.z);
                    let q = v3(
                        p.x.clamp(-inner.x, inner.x),
                        p.y.clamp(-inner.y, inner.y),
                        p.z.clamp(-inner.z, inner.z),
                    );
                    let d = p - q;
                    let nn = if d.len() > 1e-6 { d.norm() } else { nrm };
                    m.vertex(q + nn * r, nn);
                }
            }
            let w = (n + 1) as u32;
            for j in 0..n as u32 {
                for i in 0..n as u32 {
                    let a = base + j * w + i;
                    m.tri(a, a + 1, a + w + 1);
                    m.tri(a, a + w + 1, a + w);
                }
            }
        }
        m.finish()
    }

    /// Surface of revolution around Y of a profile of (radius, y) points. A repeated point
    /// makes a sharp crease. `a0..a1` is the swept angle (full turn = `0..TAU`).
    pub fn lathe(profile: &[[f32; 2]], seg: usize, a0: f32, a1: f32) -> Mesh3 {
        let mut m = Mesh3::default();
        let full = (a1 - a0 - TAU).abs() < 1e-4;
        let cols = if full { seg } else { seg + 1 };
        // Split into smooth runs at repeated points.
        let mut runs: Vec<Vec<[f32; 2]>> = vec![Vec::new()];
        for &p in profile {
            let run = runs.last_mut().expect("run");
            if let Some(&last) = run.last()
                && (last[0] - p[0]).abs() < 1e-6
                && (last[1] - p[1]).abs() < 1e-6
            {
                runs.push(vec![p]);
                continue;
            }
            run.push(p);
        }
        for run in runs.iter().filter(|r| r.len() > 1) {
            let n = run.len();
            let base = m.pos.len() as u32;
            for i in 0..n {
                let prev = run[i.saturating_sub(1)];
                let next = run[(i + 1).min(n - 1)];
                let (dr, dy) = (next[0] - prev[0], next[1] - prev[1]);
                let (nr, ny) = (dy, -dr);
                for j in 0..cols {
                    let a = a0 + (a1 - a0) * j as f32 / seg as f32;
                    let (s, c) = a.sin_cos();
                    m.vertex(
                        v3(run[i][0] * c, run[i][1], run[i][0] * s),
                        v3(nr * c, ny, nr * s),
                    );
                }
            }
            for i in 0..n as u32 - 1 {
                for j in 0..seg as u32 {
                    let j1 = if full { (j + 1) % seg as u32 } else { j + 1 };
                    let c = cols as u32;
                    let (a, b) = (base + i * c + j, base + i * c + j1);
                    let (d, e) = (base + (i + 1) * c + j, base + (i + 1) * c + j1);
                    m.tri(a, b, e);
                    m.tri(a, e, d);
                }
            }
        }
        m.finish()
    }

    /// A cylinder-like puck: radius `r`, height `h`, edges rounded by `b`.
    pub fn puck(r: f32, h: f32, b: f32, seg: usize) -> Mesh3 {
        Mesh3::lathe(&puck_profile(r, h, b, 6), seg, 0.0, TAU)
    }

    /// Torus around Y: ring radius `big`, tube radius `small`, swept over `arc` radians from
    /// angle `start`; open arcs get flat round caps.
    pub fn torus(big: f32, small: f32, seg: usize, sides: usize, start: f32, arc: f32) -> Mesh3 {
        Mesh3::ring(big, small, seg, sides, start, arc, false)
    }

    /// An open torus arc whose ends are rounded (half spheres), like a bent tube.
    pub fn tube(big: f32, small: f32, seg: usize, sides: usize, start: f32, arc: f32) -> Mesh3 {
        Mesh3::ring(big, small, seg, sides, start, arc, true)
    }

    fn ring(
        big: f32,
        small: f32,
        seg: usize,
        sides: usize,
        start: f32,
        arc: f32,
        round: bool,
    ) -> Mesh3 {
        let mut m = Mesh3::default();
        let full = (arc - TAU).abs() < 1e-4;
        let cols = if full { seg } else { seg + 1 };
        for i in 0..cols {
            let a = start + arc * i as f32 / seg as f32;
            let (s, c) = a.sin_cos();
            for j in 0..sides {
                let b = TAU * j as f32 / sides as f32;
                let (sb, cb) = b.sin_cos();
                let n = v3(cb * c, sb, cb * s);
                m.vertex(
                    v3((big + small * cb) * c, small * sb, (big + small * cb) * s),
                    n,
                );
            }
        }
        let sd = sides as u32;
        for i in 0..seg as u32 {
            let i1 = if full { (i + 1) % seg as u32 } else { i + 1 };
            for j in 0..sd {
                let j1 = (j + 1) % sd;
                let (a, b, c, d) = (i * sd + j, i1 * sd + j, i1 * sd + j1, i * sd + j1);
                m.tri(a, b, c);
                m.tri(a, c, d);
            }
        }
        if !full && round {
            let k_steps = (sides / 3).max(3) as u32;
            for (end, sign, ring0) in [(start, -1.0f32, 0u32), (start + arc, 1.0, seg as u32 * sd)]
            {
                let (s, c) = end.sin_cos();
                let tangent = v3(-s, 0.0, c) * sign;
                let centre = v3(big * c, 0.0, big * s);
                let radial = v3(c, 0.0, s);
                let mut prev: Vec<u32> = (0..sd).map(|j| ring0 + j).collect();
                for k in 1..=k_steps {
                    let phi = FRAC_PI_2 * k as f32 / k_steps as f32;
                    let (sp, cp) = phi.sin_cos();
                    let cur: Vec<u32> = (0..sides)
                        .map(|j| {
                            let b = TAU * j as f32 / sides as f32;
                            let (sb, cb) = b.sin_cos();
                            let n = (radial * cb + V3::Y * sb) * cp + tangent * sp;
                            m.vertex(centre + n * small, n)
                        })
                        .collect();
                    for j in 0..sd as usize {
                        let j1 = (j + 1) % sd as usize;
                        m.tri(prev[j], cur[j], cur[j1]);
                        m.tri(prev[j], cur[j1], prev[j1]);
                    }
                    prev = cur;
                }
            }
        } else if !full {
            for (end, sign) in [(start, -1.0f32), (start + arc, 1.0)] {
                let (s, c) = end.sin_cos();
                let tangent = v3(-s, 0.0, c) * sign;
                let center = m.vertex(v3(big * c, 0.0, big * s), tangent);
                let first = m.pos.len() as u32;
                for j in 0..sides {
                    let b = TAU * j as f32 / sides as f32;
                    let (sb, cb) = b.sin_cos();
                    m.vertex(
                        v3((big + small * cb) * c, small * sb, (big + small * cb) * s),
                        tangent,
                    );
                }
                for j in 0..sd {
                    m.tri(center, first + j, first + (j + 1) % sd);
                }
            }
        }
        m.finish()
    }

    /// A 2D outline (counter-clockwise, in XY) extruded along Z to `depth`, with the front and
    /// back edges rounded by `bevel` over `steps` rings. Corners sharper than about 35 degrees
    /// stay crisp.
    pub fn extrude(outline: &[[f32; 2]], depth: f32, bevel: f32, steps: usize) -> Mesh3 {
        let n = outline.len();
        let mut m = Mesh3::default();
        if n < 3 {
            return m;
        }
        let p: Vec<V3> = outline.iter().map(|q| v3(q[0], q[1], 0.0)).collect();
        // Outward edge normals (CCW outline: outward is (dy, -dx)).
        let en: Vec<V3> = (0..n)
            .map(|i| {
                let d = p[(i + 1) % n] - p[i];
                v3(d.y, -d.x, 0.0).norm()
            })
            .collect();
        // Miter offset direction per vertex (for insetting), limited at sharp corners.
        let miter: Vec<V3> = (0..n)
            .map(|i| {
                let a = en[(i + n - 1) % n];
                let b = en[i];
                let s = (a + b).norm();
                let c = s.dot(b).max(0.35);
                s * (1.0 / c)
            })
            .collect();
        // Wall vertices: (outline index, normal). Sharp corners get two.
        let mut wall: Vec<(usize, V3)> = Vec::new();
        for i in 0..n {
            let a = en[(i + n - 1) % n];
            let b = en[i];
            if a.dot(b) < 0.82 {
                wall.push((i, a));
                wall.push((i, b));
            } else {
                wall.push((i, (a + b).norm()));
            }
        }
        let bevel = bevel.min(depth * 0.5).max(0.0);
        let hz = depth * 0.5;
        let steps = if bevel > 0.0 { steps.max(1) } else { 0 };
        // Rings from the front cap edge (theta = 90 deg) down the front bevel, the wall, the
        // back bevel to the back cap edge.
        let mut rings: Vec<(f32, f32)> = Vec::new(); // (theta, z sign)
        for k in (0..=steps).rev() {
            rings.push((PI * 0.5 * k as f32 / steps.max(1) as f32, 1.0));
        }
        for k in 0..=steps {
            rings.push((PI * 0.5 * k as f32 / steps.max(1) as f32, -1.0));
        }
        let w = wall.len() as u32;
        let base = m.pos.len() as u32;
        for &(th, sz) in &rings {
            let (s, c) = th.sin_cos();
            for &(i, nn) in &wall {
                let pos = p[i] - miter[i] * (bevel * (1.0 - c));
                let z = sz * (hz - bevel + bevel * s);
                m.vertex(v3(pos.x, pos.y, z), v3(nn.x * c, nn.y * c, sz * s));
            }
        }
        for r in 0..rings.len() as u32 - 1 {
            for j in 0..w {
                let j1 = (j + 1) % w;
                if wall[j as usize].0 == wall[j1 as usize].0 {
                    continue;
                }
                let (a, b) = (base + r * w + j, base + r * w + j1);
                let (c, d) = (base + (r + 1) * w + j, base + (r + 1) * w + j1);
                m.tri(a, c, d);
                m.tri(a, d, b);
            }
        }
        // Caps.
        let inset: Vec<[f32; 2]> = (0..n)
            .map(|i| {
                let q = p[i] - miter[i] * bevel;
                [q.x, q.y]
            })
            .collect();
        let tris = triangulate(&inset);
        for sz in [1.0f32, -1.0] {
            let cb = m.pos.len() as u32;
            for q in &inset {
                m.vertex(v3(q[0], q[1], sz * hz), v3(0.0, 0.0, sz));
            }
            for t in &tris {
                m.tri(cb + t[0] as u32, cb + t[1] as u32, cb + t[2] as u32);
            }
        }
        m.finish()
    }

    /// A faceted gem around Y: a table on top, a crown, a girdle of radius `r` and a pointed
    /// pavilion below.
    pub fn gem(sides: usize, r: f32, crown: f32, pavilion: f32) -> Mesh3 {
        let profile = [
            [0.0, crown],
            [r * 0.55, crown],
            [r, crown * 0.25],
            [r, 0.0],
            [0.0, -pavilion],
        ];
        // Offset every other ring by half a step for a brilliant-like facet pattern.
        let mut m = Mesh3::default();
        let ring = |k: usize, i: usize| -> V3 {
            let off = if k % 2 == 1 { 0.5 } else { 0.0 };
            let a = TAU * (i as f32 + off) / sides as f32;
            let [rr, y] = profile[k];
            v3(rr * a.cos(), y, rr * a.sin())
        };
        for k in 0..profile.len() - 1 {
            for i in 0..sides {
                let a = ring(k, i);
                let b = ring(k, (i + 1) % sides);
                let c = ring(k + 1, i);
                let d = ring(k + 1, (i + 1) % sides);
                for t in [[a, b, d], [a, d, c]] {
                    let n = (t[1] - t[0]).cross(t[2] - t[0]);
                    if n.len() < 1e-9 {
                        continue;
                    }
                    let mid = (t[0] + t[1] + t[2]) * (1.0 / 3.0);
                    let out = v3(mid.x, mid.y * 0.3, mid.z);
                    let n = if n.dot(out) < 0.0 { -n } else { n };
                    let i0 = m.vertex(t[0], n);
                    m.vertex(t[1], n);
                    m.vertex(t[2], n);
                    m.tri(i0, i0 + 1, i0 + 2);
                }
            }
        }
        m.finish()
    }
}

/// Profile of a puck (bottom centre to top centre) with rounded edges.
pub fn puck_profile(r: f32, h: f32, b: f32, steps: usize) -> Vec<[f32; 2]> {
    let b = b.min(r).min(h * 0.5);
    let hy = h * 0.5;
    let mut p = vec![[0.0, -hy], [r - b, -hy]];
    for k in 1..steps {
        let a = -PI * 0.5 + PI * 0.5 * k as f32 / steps as f32;
        p.push([r - b + b * a.cos(), -hy + b + b * a.sin()]);
    }
    p.push([r, -hy + b]);
    p.push([r, hy - b]);
    for k in 1..steps {
        let a = PI * 0.5 * k as f32 / steps as f32;
        p.push([r - b + b * a.cos(), hy - b + b * a.sin()]);
    }
    p.push([r - b, hy]);
    p.push([0.0, hy]);
    p
}

/// Ear-clipping triangulation of a simple polygon (either winding); returns index triples
/// counter-clockwise.
pub fn triangulate(poly: &[[f32; 2]]) -> Vec<[usize; 3]> {
    let n = poly.len();
    if n < 3 {
        return Vec::new();
    }
    let area: f32 = (0..n)
        .map(|i| {
            let (a, b) = (poly[i], poly[(i + 1) % n]);
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    let mut idx: Vec<usize> = (0..n).collect();
    if area < 0.0 {
        idx.reverse();
    }
    let cross = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    };
    let mut out = Vec::with_capacity(n - 2);
    let mut guard = 0;
    while idx.len() > 3 && guard < n * n {
        guard += 1;
        let m = idx.len();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, c) = (poly[ia], poly[ib], poly[ic]);
            if cross(a, b, c) <= 1e-9 {
                continue;
            }
            let inside = idx.iter().any(|&j| {
                if j == ia || j == ib || j == ic {
                    return false;
                }
                let p = poly[j];
                cross(a, b, p) >= 0.0 && cross(b, c, p) >= 0.0 && cross(c, a, p) >= 0.0
            });
            if inside {
                continue;
            }
            out.push([ia, ib, ic]);
            idx.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            break;
        }
    }
    if idx.len() == 3 {
        out.push([idx[0], idx[1], idx[2]]);
    } else {
        // Degenerate remainder: fan it.
        for i in 1..idx.len().saturating_sub(1) {
            out.push([idx[0], idx[i], idx[i + 1]]);
        }
    }
    out
}

// ---------------------------------------------------------------- scene

/// One mesh placed in the world with a material.
#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    pub mesh: &'a Mesh3,
    pub xf: Xf,
    pub mat: Material,
    /// Multiplies the material alpha (fading whole objects in and out).
    pub fade: f32,
    /// Moves the object's sorting depth towards the camera (world units), for parts that sit
    /// on or around another part (decals, a gauge over its track).
    pub bias: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpriteKind {
    /// Additive soft light.
    Glow,
    /// A small soft-edged disc (normal blending).
    Dot,
    /// Four-point glint rotated by the angle (additive).
    Star(f32),
    /// A soft dark ellipse on the floor, `squash` = height / width.
    Shadow(f32),
}

/// A camera-facing 2D element at a 3D position (radius in world units).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sprite {
    pub pos: V3,
    pub radius: f32,
    pub color: Lin,
    pub alpha: f32,
    pub kind: SpriteKind,
}

/// Everything one frame draws.
pub struct Scene<'a> {
    pub cam: Camera,
    pub rig: Rig,
    pub items: Vec<Item<'a>>,
    pub sprites: Vec<Sprite>,
}

/// Counters from the last render (for budgets and tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub triangles: usize,
    pub silhouette_edges: usize,
    pub sprites: usize,
}

#[derive(Clone, Copy)]
enum Prim {
    /// First index into the flat triangle list (three indices).
    Tri(u32),
    /// Feather strip: four vertices starting here.
    Strip(u32),
    Sprite(u32),
    /// An opaque object's primitives, already sorted among themselves.
    Group(u32),
}

impl<'a> Scene<'a> {
    pub fn new(cam: Camera, rig: Rig) -> Scene<'a> {
        Scene {
            cam,
            rig,
            items: Vec::new(),
            sprites: Vec::new(),
        }
    }

    pub fn add(&mut self, mesh: &'a Mesh3, xf: Xf, mat: Material) {
        self.items.push(Item {
            mesh,
            xf,
            mat,
            fade: 1.0,
            bias: 0.0,
        });
    }

    pub fn add_faded(&mut self, mesh: &'a Mesh3, xf: Xf, mat: Material, fade: f32) {
        if fade > 0.003 {
            self.items.push(Item {
                mesh,
                xf,
                mat,
                fade,
                bias: 0.0,
            });
        }
    }

    /// Sort the last added object as if it were `b` world units nearer.
    pub fn bias_last(&mut self, b: f32) {
        if let Some(i) = self.items.last_mut() {
            i.bias = b;
        }
    }

    pub fn sprite(&mut self, pos: V3, radius: f32, color: Lin, alpha: f32, kind: SpriteKind) {
        if alpha > 0.003 && radius > 0.0 {
            self.sprites.push(Sprite {
                pos,
                radius,
                color,
                alpha,
                kind,
            });
        }
    }

    /// Render into an egui mesh covering `rect`; `ppp` sets the feather width (one physical
    /// pixel).
    pub fn render(&self, rect: Rect, ppp: f32) -> (Mesh, Stats) {
        let view = View::new(&self.cam, rect);
        let feather = 1.0 / ppp.max(0.25);
        let mut out = Mesh::default();
        let mut tri_idx: Vec<u32> = Vec::new();
        let mut prims: Vec<(f32, Prim)> = Vec::new();
        let mut groups: Vec<Vec<(f32, Prim)>> = Vec::new();
        let mut stats = Stats::default();
        let lights = &self.rig.lights;
        for item in &self.items {
            let mesh = item.mesh;
            let mat = &item.mat;
            let base = out.vertices.len() as u32;
            let nm = item.xf.m.normal_matrix();
            let glass = mat.alpha < 0.999;
            let fade = item.fade.clamp(0.0, 1.0);
            // Opaque objects sort as a whole (by their centre) so parts resting on each other
            // never interleave; glass sorts per triangle so it wraps what is inside it.
            let mut local: Vec<(f32, Prim)> = Vec::new();
            let mut depth: Vec<f32> = Vec::with_capacity(mesh.pos.len());
            let mut screen: Vec<Pos2> = Vec::with_capacity(mesh.pos.len());
            for (i, &p) in mesh.pos.iter().enumerate() {
                let wp = item.xf.apply(p);
                let n = nm.apply(mesh.nrm[i]).norm();
                let (sp, z) = view.project(wp);
                let v = (view.eye - wp).norm();
                let ndv = n.dot(v).max(0.0);
                let base_col = mix(mat.bottom, mat.top, mesh.tint[i]);
                let hemi = mix(self.rig.ground, self.rig.sky, n.y * 0.5 + 0.5);
                let mut diff = hemi;
                let mut spec = [0.0f32; 3];
                for l in lights {
                    let d = n.dot(l.dir).max(0.0) * mat.diffuse;
                    for (dk, ck) in diff.iter_mut().zip(l.col) {
                        *dk += ck * d;
                    }
                    if l.spec && mat.spec > 0.0 {
                        let h = (l.dir + v).norm();
                        let s = n.dot(h).max(0.0).powf(mat.shine) * mat.spec;
                        for (sk, ck) in spec.iter_mut().zip(l.col) {
                            *sk += ck * s;
                        }
                    }
                }
                let fres = (1.0 - ndv).powi(3);
                let mut c = [0.0f32; 3];
                for k in 0..3 {
                    c[k] = base_col[k] * diff[k]
                        + spec[k]
                        + mat.rim_col[k] * mat.rim * fres
                        + mat.emissive[k];
                }
                let a = if glass {
                    (mat.alpha + (1.0 - mat.alpha) * fres * mat.fresnel_alpha).min(1.0) * fade
                } else {
                    fade
                };
                out.vertices.push(Vertex {
                    pos: sp,
                    uv: WHITE_UV,
                    color: out_color(c, a),
                });
                depth.push(z);
                screen.push(sp);
            }
            // Facing per triangle (screen-space winding; y points down).
            let front: Vec<bool> = mesh
                .tris
                .iter()
                .map(|t| {
                    let [a, b, c] = t.map(|i| screen[i as usize]);
                    (b - a).x * (c - a).y - (b - a).y * (c - a).x < 0.0
                })
                .collect();
            for (ti, t) in mesh.tris.iter().enumerate() {
                if !front[ti] && !glass {
                    continue;
                }
                let z = (depth[t[0] as usize] + depth[t[1] as usize] + depth[t[2] as usize]) / 3.0;
                let at = tri_idx.len() as u32;
                tri_idx.extend(t.iter().map(|&i| base + i));
                local.push((z, Prim::Tri(at)));
                stats.triangles += 1;
            }
            // Silhouette feather strips.
            for e in &mesh.edges {
                let (t0, t1) = (e[2] as usize, e[3]);
                let f0 = front[t0];
                let f1 = if t1 == NONE {
                    false
                } else {
                    front[t1 as usize]
                };
                if f0 == f1 {
                    continue;
                }
                let ft = if f0 { t0 } else { t1 as usize };
                let tri = mesh.tris[ft];
                // The edge as it appears in the front triangle, plus its third vertex.
                let (a, b) = (e[0], e[1]);
                let Some(third) = tri.iter().copied().find(|&v| {
                    let p = mesh.pos[v as usize];
                    (p - mesh.pos[a as usize]).len() > 1e-6
                        && (p - mesh.pos[b as usize]).len() > 1e-6
                }) else {
                    continue;
                };
                let (sa, sb, sc) = (
                    screen[a as usize],
                    screen[b as usize],
                    screen[third as usize],
                );
                let d = sb - sa;
                let len = d.length();
                if len < 1e-4 {
                    continue;
                }
                let mut nrm = egui::vec2(-d.y, d.x) / len;
                if nrm.dot(sc - sa) > 0.0 {
                    nrm = -nrm;
                }
                let off = nrm * feather;
                let ca = out.vertices[(base + a) as usize].color;
                let cb = out.vertices[(base + b) as usize].color;
                let first = out.vertices.len() as u32;
                for (pos, color) in [
                    (sa, ca),
                    (sb, cb),
                    (sb + off, Color32::TRANSPARENT),
                    (sa + off, Color32::TRANSPARENT),
                ] {
                    out.vertices.push(Vertex {
                        pos,
                        uv: WHITE_UV,
                        color,
                    });
                }
                let z = (depth[tri[0] as usize] + depth[tri[1] as usize] + depth[tri[2] as usize])
                    / 3.0;
                local.push((z - 1e-5, Prim::Strip(first)));
                stats.silhouette_edges += 1;
            }
            if glass {
                prims.append(&mut local);
            } else {
                local.sort_by(|a, b| b.0.total_cmp(&a.0));
                let (_, z) = view.project(item.xf.apply(mesh.center));
                prims.push((z - item.bias, Prim::Group(groups.len() as u32)));
                groups.push(local);
            }
        }
        for (i, s) in self.sprites.iter().enumerate() {
            let (_, z) = view.project(s.pos);
            prims.push((z, Prim::Sprite(i as u32)));
            stats.sprites += 1;
        }
        // Far first; ties keep insertion order.
        prims.sort_by(|a, b| b.0.total_cmp(&a.0));
        out.indices.reserve(tri_idx.len() + prims.len() * 6);
        let flat: Vec<Prim> = prims
            .into_iter()
            .flat_map(|(_, p)| match p {
                Prim::Group(g) => groups[g as usize].iter().map(|q| q.1).collect::<Vec<_>>(),
                p => vec![p],
            })
            .collect();
        for p in flat {
            match p {
                Prim::Tri(at) => {
                    let at = at as usize;
                    out.indices.extend_from_slice(&tri_idx[at..at + 3]);
                }
                Prim::Strip(f) => {
                    out.indices
                        .extend_from_slice(&[f, f + 1, f + 2, f, f + 2, f + 3]);
                }
                Prim::Sprite(i) => {
                    let s = self.sprites[i as usize];
                    let (c, z) = view.project(s.pos);
                    let r = s.radius * view.focal / z;
                    if let Some((r, alpha)) = fit_sprite(c, r, s.alpha, s.kind, rect) {
                        let s = Sprite { alpha, ..s };
                        sprite_mesh(&mut out, c, r, &s, feather);
                    }
                }
                Prim::Group(_) => {}
            }
        }
        (out, stats)
    }
}

/// Soft sprites end this far inside the rect, in points, so a clipped slot never shows
/// the edge of a glow as a box (REVIEW-B B3).
pub const SPRITE_MARGIN: f32 = 8.0;

/// Keeps a sprite inside `rect` less [`SPRITE_MARGIN`]: glows and shadows shrink their
/// radius (their falloff still reaches zero alpha), dots and glints fade out as they
/// near the margin. Continuous in the position, so moving sprites never pop.
fn fit_sprite(c: Pos2, r: f32, alpha: f32, kind: SpriteKind, rect: Rect) -> Option<(f32, f32)> {
    let margin = SPRITE_MARGIN.min(rect.width().min(rect.height()) * 0.1);
    let room_x = (c.x - rect.left()).min(rect.right() - c.x) - margin;
    let room_y = (c.y - rect.top()).min(rect.bottom() - c.y) - margin;
    let (r, alpha) = match kind {
        // A shrunk glow also dims, so it stays a soft halo and never turns into a spot.
        SpriteKind::Glow => {
            let fit = r.min(room_x).min(room_y);
            (fit, alpha * (fit / r.max(1e-3)).clamp(0.0, 1.0).sqrt())
        }
        SpriteKind::Shadow(squash) => (r.min(room_x).min(room_y / squash.max(0.01)), alpha),
        SpriteKind::Dot | SpriteKind::Star(_) => {
            let room = room_x.min(room_y) - r;
            (r, alpha * (room / 6.0).clamp(0.0, 1.0))
        }
    };
    (r > 0.5 && alpha > 0.0).then_some((r, alpha))
}

fn fan(out: &mut Mesh, c: Pos2, rings: &[(f32, f32, Color32)], seg: usize) {
    // rings: (radius x, radius y, colour); first ring may have radius 0 (centre).
    let base = out.vertices.len() as u32;
    for &(rx, ry, col) in rings {
        for j in 0..seg {
            let a = TAU * j as f32 / seg as f32;
            out.vertices.push(Vertex {
                pos: c + egui::vec2(a.cos() * rx, a.sin() * ry),
                uv: WHITE_UV,
                color: col,
            });
        }
    }
    let s = seg as u32;
    for r in 0..rings.len() as u32 - 1 {
        for j in 0..s {
            let j1 = (j + 1) % s;
            let (a, b) = (base + r * s + j, base + r * s + j1);
            let (d, e) = (base + (r + 1) * s + j, base + (r + 1) * s + j1);
            out.indices.extend_from_slice(&[a, b, e, a, e, d]);
        }
    }
}

fn sprite_mesh(out: &mut Mesh, c: Pos2, r: f32, s: &Sprite, feather: f32) {
    let seg = ((r * 0.8) as usize).clamp(12, 48);
    match s.kind {
        SpriteKind::Glow => {
            let col = |k: f32| additive(s.color, s.alpha * k);
            fan(
                out,
                c,
                &[
                    (0.0, 0.0, col(1.0)),
                    (r * 0.2, r * 0.2, col(0.8)),
                    (r * 0.45, r * 0.45, col(0.38)),
                    (r * 0.7, r * 0.7, col(0.1)),
                    (r * 0.88, r * 0.88, col(0.02)),
                    (r, r, col(0.0)),
                ],
                seg,
            );
        }
        SpriteKind::Dot => {
            let col = out_color(s.color, s.alpha);
            let edge = feather.max(r * 0.25);
            fan(
                out,
                c,
                &[
                    (0.0, 0.0, col),
                    ((r - edge * 0.5).max(0.0), (r - edge * 0.5).max(0.0), col),
                    (r + edge * 0.5, r + edge * 0.5, Color32::TRANSPARENT),
                ],
                seg,
            );
        }
        SpriteKind::Shadow(squash) => {
            let col = |k: f32| Color32::from_black_alpha((s.alpha * k * 255.0) as u8);
            fan(
                out,
                c,
                &[
                    (0.0, 0.0, col(1.0)),
                    (r * 0.35, r * 0.35 * squash, col(0.8)),
                    (r * 0.7, r * 0.7 * squash, col(0.3)),
                    (r, r * squash, col(0.0)),
                ],
                seg,
            );
        }
        SpriteKind::Star(rot) => {
            let core = additive(s.color, s.alpha);
            // Four thin spikes: a diamond per spike (centre, side, tip, side).
            for k in 0..4 {
                let a = rot + k as f32 * PI * 0.5;
                let len = if k % 2 == 0 { r } else { r * 0.62 };
                let dir = egui::vec2(a.cos(), a.sin());
                let side = egui::vec2(-dir.y, dir.x) * (r * 0.09).max(feather);
                let b = out.vertices.len() as u32;
                for (p, col) in [
                    (c, core),
                    (
                        c + dir * len * 0.22 + side,
                        additive(s.color, s.alpha * 0.55),
                    ),
                    (c + dir * len, Color32::TRANSPARENT),
                    (
                        c + dir * len * 0.22 - side,
                        additive(s.color, s.alpha * 0.55),
                    ),
                ] {
                    out.vertices.push(Vertex {
                        pos: p,
                        uv: WHITE_UV,
                        color: col,
                    });
                }
                out.indices
                    .extend_from_slice(&[b, b + 1, b + 2, b, b + 2, b + 3]);
            }
            let col = |k: f32| additive(s.color, s.alpha * k);
            fan(
                out,
                c,
                &[
                    (0.0, 0.0, col(0.9)),
                    (r * 0.12, r * 0.12, col(0.5)),
                    (r * 0.4, r * 0.4, col(0.0)),
                ],
                16,
            );
        }
    }
}

// ---------------------------------------------------------------- CPU rasteriser

/// Rasterise an egui mesh (white texture only) into straight-alpha RGBA8 of `w` x `h` pixels,
/// where one point is `scale` pixels, with `ss` x `ss` supersampling. Blending follows egui
/// (premultiplied, in gamma space), so the result looks like the GPU's, minus aliasing.
pub fn rasterize(mesh: &Mesh, w: usize, h: usize, scale: f32, ss: usize) -> Vec<u8> {
    let ss = ss.max(1);
    let sw = w * ss;
    let k = scale * ss as f32;
    let col = |c: Color32| {
        [
            c.r() as f32 / 255.0,
            c.g() as f32 / 255.0,
            c.b() as f32 / 255.0,
            c.a() as f32 / 255.0,
        ]
    };
    // Triangles in sample space with their bounding boxes.
    let tris: Vec<([(f32, f32); 3], [[f32; 4]; 3], f32, [f32; 4])> = mesh
        .indices
        .as_chunks::<3>()
        .0
        .iter()
        .filter_map(|t| {
            let v = [0, 1, 2].map(|i| &mesh.vertices[t[i] as usize]);
            let p = v.map(|v| (v.pos.x * k, v.pos.y * k));
            let area =
                (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
            if area.abs() < 1e-9 {
                return None;
            }
            let bb = [
                p.iter().map(|q| q.0).fold(f32::MAX, f32::min),
                p.iter().map(|q| q.1).fold(f32::MAX, f32::min),
                p.iter().map(|q| q.0).fold(f32::MIN, f32::max),
                p.iter().map(|q| q.1).fold(f32::MIN, f32::max),
            ];
            Some((p, v.map(|v| col(v.color)), area, bb))
        })
        .collect();
    let mut px = vec![0u8; w * h * 4];
    // Work in bands of output rows to keep memory small at large sizes.
    let band = 32usize;
    let mut buf = vec![[0f32; 4]; sw * band * ss];
    let n = (ss * ss) as f32;
    for y0 in (0..h).step_by(band) {
        let rows = band.min(h - y0);
        let (sy0, sy1) = (y0 * ss, (y0 + rows) * ss);
        buf.iter_mut().for_each(|b| *b = [0.0; 4]);
        for (p, c, area, bb) in &tris {
            if bb[3] < sy0 as f32 || bb[1] > sy1 as f32 {
                continue;
            }
            let minx = bb[0].floor().max(0.0) as usize;
            let maxx = (bb[2].ceil().max(0.0) as usize).min(sw);
            let miny = (bb[1].floor().max(sy0 as f32) as usize).max(sy0);
            let maxy = (bb[3].ceil().max(0.0) as usize).min(sy1);
            for y in miny..maxy {
                let py = y as f32 + 0.5;
                for x in minx..maxx {
                    let qx = x as f32 + 0.5;
                    let e = |a: (f32, f32), b: (f32, f32)| {
                        (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (qx - a.0)
                    };
                    let w0 = e(p[1], p[2]) / area;
                    let w1 = e(p[2], p[0]) / area;
                    let w2 = e(p[0], p[1]) / area;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }
                    let s = [0, 1, 2, 3].map(|i| c[0][i] * w0 + c[1][i] * w1 + c[2][i] * w2);
                    let d = &mut buf[(y - sy0) * sw + x];
                    for i in 0..4 {
                        d[i] = s[i] + d[i] * (1.0 - s[3]);
                    }
                }
            }
        }
        for yy in 0..rows {
            for x in 0..w {
                let mut acc = [0f32; 4];
                for sy in 0..ss {
                    for sx in 0..ss {
                        let s = buf[(yy * ss + sy) * sw + x * ss + sx];
                        for i in 0..4 {
                            acc[i] += s[i].clamp(0.0, 1.0);
                        }
                    }
                }
                let a = acc[3] / n;
                let o = ((y0 + yy) * w + x) * 4;
                for i in 0..3 {
                    // Additive light over transparency cannot be stored: clamp to coverage.
                    let v = if a > 1e-6 {
                        acc[i].min(acc[3]) / n / a
                    } else {
                        0.0
                    };
                    px[o + i] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                }
                px[o + 3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(m: &Mesh3) -> bool {
        m.edges.iter().all(|e| e[3] != NONE)
    }

    fn outward(m: &Mesh3) -> f32 {
        // Share of triangles whose geometric normal agrees with the vertex normals.
        let ok = m
            .tris
            .iter()
            .filter(|t| {
                let [a, b, c] = t.map(|i| m.pos[i as usize]);
                let g = (b - a).cross(c - a);
                let n = m.nrm[t[0] as usize] + m.nrm[t[1] as usize] + m.nrm[t[2] as usize];
                g.dot(n) > 0.0
            })
            .count();
        ok as f32 / m.tris.len().max(1) as f32
    }

    fn centroid_outward(m: &Mesh3) -> f32 {
        // For convex-ish solids around the origin: normals point away from the centre.
        let ok = m
            .pos
            .iter()
            .zip(&m.nrm)
            .filter(|(p, n)| p.len() < 1e-4 || p.dot(**n) > -1e-4)
            .count();
        ok as f32 / m.pos.len() as f32
    }

    #[test]
    fn generators_make_closed_outward_meshes() {
        let star: Vec<[f32; 2]> = (0..10)
            .map(|i| {
                let a = TAU * i as f32 / 10.0;
                let r = if i % 2 == 0 { 1.0 } else { 0.5 };
                [r * a.cos(), r * a.sin()]
            })
            .collect();
        let meshes = [
            ("icosphere", Mesh3::icosphere(2)),
            ("box", Mesh3::rounded_box(v3(1.0, 0.6, 0.4), 0.15, 7)),
            ("puck", Mesh3::puck(1.0, 0.5, 0.15, 48)),
            ("torus", Mesh3::torus(1.0, 0.25, 48, 16, 0.0, TAU)),
            ("arc", Mesh3::torus(1.0, 0.25, 48, 16, 0.3, 4.0)),
            ("tube", Mesh3::tube(1.0, 0.25, 48, 16, 0.3, 4.0)),
            ("extrude", Mesh3::extrude(&star, 0.4, 0.06, 3)),
            ("gem", Mesh3::gem(8, 1.0, 0.5, 1.0)),
        ];
        for (name, m) in &meshes {
            assert!(!m.tris.is_empty(), "{name}");
            assert!(closed(m), "{name} has open edges");
            assert!(outward(m) > 0.999, "{name}: {}", outward(m));
            if !["torus", "arc", "tube", "extrude"].contains(name) {
                assert!(
                    centroid_outward(m) > 0.99,
                    "{name}: {}",
                    centroid_outward(m)
                );
            }
            assert_eq!(m.tint.len(), m.pos.len());
        }
    }

    #[test]
    fn triangulates_concave_outlines() {
        let l = [
            [0.0, 0.0],
            [2.0, 0.0],
            [2.0, 1.0],
            [1.0, 1.0],
            [1.0, 2.0],
            [0.0, 2.0],
        ];
        let t = triangulate(&l);
        assert_eq!(t.len(), 4);
        let area: f32 = t
            .iter()
            .map(|t| {
                let (a, b, c) = (l[t[0]], l[t[1]], l[t[2]]);
                0.5 * ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]))
            })
            .sum();
        assert!((area - 3.0).abs() < 1e-5, "{area}");
        // Clockwise input gives the same area, counter-clockwise triangles.
        let mut cw = l;
        cw.reverse();
        assert_eq!(triangulate(&cw).len(), 4);
    }

    #[test]
    fn transforms_compose_in_order() {
        let x = Xf::scale(2.0).rot_z(PI * 0.5).at(v3(1.0, 0.0, 0.0));
        let p = x.apply(v3(1.0, 0.0, 0.0));
        assert!((p - v3(1.0, 2.0, 0.0)).len() < 1e-5, "{p:?}");
        let y = Xf::ID.at(v3(0.0, 1.0, 0.0)).then(Xf::ID.rot_x(PI));
        let q = y.apply(V3::ZERO);
        assert!((q - v3(0.0, -1.0, 0.0)).len() < 1e-5, "{q:?}");
        // Normal matrix of a non-uniform scale keeps normals perpendicular.
        let m = M3::scale(v3(2.0, 1.0, 1.0));
        let n = m.normal_matrix().apply(v3(1.0, 1.0, 0.0).norm()).norm();
        let tangent = m.apply(v3(1.0, -1.0, 0.0));
        assert!(n.dot(tangent).abs() < 1e-5);
    }

    fn sphere_scene(m: &Mesh3) -> Scene<'_> {
        let mut s = Scene::new(Camera::front(5.0, 0.2), Rig::studio([0.3, 0.8, 0.7]));
        s.add(m, Xf::ID, Material::gloss([0.2, 0.7, 0.6], [0.1, 0.3, 0.6]));
        s
    }

    #[test]
    fn culls_back_faces_and_feathers_the_silhouette() {
        let m = Mesh3::icosphere(3);
        let (mesh, stats) = sphere_scene(&m).render(
            Rect::from_min_size(Pos2::ZERO, egui::vec2(200.0, 200.0)),
            2.0,
        );
        // About half the sphere faces the camera (a bit less in perspective).
        let total = m.tris.len();
        assert!(
            stats.triangles > total / 3 && stats.triangles < total * 3 / 5,
            "{stats:?}"
        );
        // The silhouette is one closed loop around the sphere.
        assert!(
            stats.silhouette_edges > 20 && stats.silhouette_edges < 200,
            "{stats:?}"
        );
        assert!(mesh.indices.len() >= stats.triangles * 3 + stats.silhouette_edges * 6);
        // Everything is inside the rect.
        for v in &mesh.vertices {
            assert!(v.pos.x > 0.0 && v.pos.x < 200.0 && v.pos.y > 0.0 && v.pos.y < 200.0);
        }
    }

    #[test]
    fn lit_side_is_brighter_and_highlight_is_on_the_key_side() {
        let m = Mesh3::icosphere(3);
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(100.0, 100.0));
        let (mesh, _) = sphere_scene(&m).render(rect, 1.0);
        let px = rasterize(&mesh, 100, 100, 1.0, 2);
        let lum = |x: usize, y: usize| {
            let o = (y * 100 + x) * 4;
            px[o] as u32 + px[o + 1] as u32 + px[o + 2] as u32
        };
        // Key light comes from the upper left.
        assert!(
            lum(38, 38) > lum(62, 62) + 60,
            "{} vs {}",
            lum(38, 38),
            lum(62, 62)
        );
        // Outside the sphere stays empty, the centre is opaque.
        assert_eq!(px[3], 0);
        assert_eq!(px[(50 * 100 + 50) * 4 + 3], 255);
    }

    #[test]
    fn painter_order_puts_near_objects_on_top() {
        let a = Mesh3::icosphere(2);
        let mut s = Scene::new(Camera::front(6.0, 0.0), Rig::studio(WHITE));
        let red = Material::satin([0.9, 0.05, 0.05], [0.9, 0.05, 0.05]);
        let blue = Material::satin([0.05, 0.05, 0.9], [0.05, 0.05, 0.9]);
        // Near red added first, far blue second: sorting must still draw red on top.
        s.add(&a, Xf::scale(0.6).at(v3(0.0, 0.0, 1.0)), red);
        s.add(&a, Xf::scale(1.0).at(v3(0.0, 0.0, -1.0)), blue);
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(80.0, 80.0));
        let (mesh, _) = s.render(rect, 1.0);
        let px = rasterize(&mesh, 80, 80, 1.0, 1);
        let o = (40 * 80 + 40) * 4;
        assert!(px[o] > px[o + 2], "centre is red: {:?}", &px[o..o + 4]);
    }

    #[test]
    fn rasterizer_supersamples_edges() {
        let m = Mesh3::icosphere(3);
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(64.0, 64.0));
        let (mesh, _) = sphere_scene(&m).render(rect, 4.0);
        let px = rasterize(&mesh, 64, 64, 1.0, 4);
        let partial = px
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 10 && p[3] < 245)
            .count();
        assert!(partial > 40, "soft edge pixels: {partial}");
    }

    #[test]
    fn soft_sprites_end_inside_the_rect() {
        // A glow much larger than the rect, a floor shadow, and dots near the edges.
        let mut s = Scene::new(Camera::front(5.0, 0.2), Rig::studio([0.3, 0.8, 0.7]));
        s.sprite(
            v3(0.0, 0.0, -1.0),
            6.0,
            [1.0, 0.8, 0.4],
            1.0,
            SpriteKind::Glow,
        );
        s.sprite(
            v3(0.0, -1.3, 0.0),
            4.0,
            [0.0; 3],
            0.6,
            SpriteKind::Shadow(0.25),
        );
        s.sprite(v3(1.9, 1.9, 0.0), 0.1, [1.0; 3], 1.0, SpriteKind::Dot);
        s.sprite(
            v3(-1.9, 0.0, 0.0),
            0.1,
            [1.0; 3],
            1.0,
            SpriteKind::Star(0.3),
        );
        let side = 100usize;
        let rect = Rect::from_min_size(Pos2::ZERO, egui::vec2(side as f32, side as f32));
        let mut all = Mesh::default();
        all.add_colored_rect(rect, Color32::BLACK);
        all.append(s.render(rect, 1.0).0);
        let px = rasterize(&all, side, side, 1.0, 2);
        let m = SPRITE_MARGIN as usize;
        for y in 0..side {
            for x in 0..side {
                if x < m || y < m || x >= side - m || y >= side - m {
                    let p = &px[(y * side + x) * 4..(y * side + x) * 4 + 3];
                    assert!(p.iter().all(|&v| v <= 1), "({x}, {y}) = {p:?}");
                }
            }
        }
        let c = (side / 2 * side + side / 2) * 4;
        assert!(px[c] > 40, "the glow is still there: {:?}", &px[c..c + 4]);
    }
}
