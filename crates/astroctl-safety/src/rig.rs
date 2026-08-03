//! Layer 2 — where the metal is, and what it can hit (SDD §5.4.3).
//!
//! Layer 1 answers "which way does declination move". This answers "will the tube reach the
//! tripod", which no celestial coordinate can express: 180° of right ascension at the home pose is
//! *zero* change in where the tube points and a half-turn of the whole assembly. Every limit above
//! this one is blind to that by construction.
//!
//! # The model, and why each piece is the shape it is
//!
//! **The moving assembly is a capsule** — a line segment with a radius — lying along the optical
//! axis, centred on the declination axis at `dec_axis_offset_mm` from the polar axis. The operator
//! first proposed a sphere centred on the declination axis, and the arithmetic ruled it out twice
//! over: a sphere centred *on* an axis is invariant under rotation about that axis, so it cannot
//! see a declination slew at all — the exact motion that put a tube into the tripod on 2026-08-01
//! — and at a radius large enough to enclose an eight-inch Newtonian it hangs 400–600 mm below the
//! mount head at every pose, which is inside the tripod always. It would have been a constant, not
//! a predicate. A capsule keeps the operator's parameters and adds the one degree of freedom that
//! makes them mean something: the far end sweeps as declination turns.
//!
//! **The tripod is a truncated cone**, narrow at the head and widening to the ground, because that
//! is the shape of a tripod and a cylinder is not. It is rotationally symmetric rather than a true
//! three-sided pyramid on purpose: the legs' azimuths change every time the rig is set up, a
//! configured azimuth would be stale the first time somebody nudged a leg, and a wrong one is worse
//! than none. The cone circumscribes the pyramid, so it refuses some poses that would in fact have
//! cleared a gap between two legs. That is the error worth having.
//!
//! **The counterweight is a second capsule along `-d̂`**, from the axes' intersection to the
//! shaft's tip, inflated by the largest weight's radius over its whole length (weights slide, so
//! any point may carry one). It swings on the other side of the declination axis and is a real
//! collision hazard during right-ascension motion — 180° of RA at the home pose is zero celestial
//! change and a half-turn of exactly this. It is optional in the configuration and absent means
//! *no* counterweight capsule, for the same reason `mount.geometry` itself ships absent: a guessed
//! length would be a limit enforced against a number that came from no measurement — the objection
//! that removed `park_position` in M3-T07.
//!
//! **The camera stack is a third capsule along `+d̂`**, rooted at the focuser's station on the
//! optical axis — "parallel to the dec axis, in front of the mount", the operator's words, and
//! the fact that retired the fat-tube approximation: sweeping the stack's reach into
//! `tube_radius_mm` covered a full ring of orientations the camera occupies exactly one of, and
//! the difference was a third of the RA turn refused at dec-home (E16's aftermath, 2026-08-02/03).
//! Optional on the same no-guessed-limits terms as the counterweight.

use astroctl_core::config::RigGeometry;
use astroctl_core::types::PierSide;

use crate::horizontal::Site;

/// How finely the capsule's axis is sampled when testing it against the tripod.
///
/// The test is "does any point of the segment lie inside the tripod, inflated by the capsule
/// radius". Sampling is exact enough when the spacing is well under the radius that inflates the
/// obstacle, because a gap can only be missed if the solid is thinner than the step: at 64 samples
/// an 900 mm tube steps 14 mm against a radius of ~120 mm, an order of magnitude of margin. The
/// alternative — a closed-form segment-to-frustum distance — is a page of case analysis for a
/// result this cannot get wrong.
const SAMPLES: usize = 64;

/// A three-dimensional vector in the local horizontal frame: east, north, up. Millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Vec3 {
    e: f64,
    n: f64,
    u: f64,
}

impl Vec3 {
    const fn new(e: f64, n: f64, u: f64) -> Self {
        Self { e, n, u }
    }

    fn cross(self, o: Self) -> Self {
        Self::new(
            self.n * o.u - self.u * o.n,
            self.u * o.e - self.e * o.u,
            self.e * o.n - self.n * o.e,
        )
    }

    fn norm(self) -> f64 {
        self.e
            .mul_add(self.e, self.n.mul_add(self.n, self.u * self.u))
            .sqrt()
    }

    fn unit(self) -> Option<Self> {
        let n = self.norm();
        (n > 1e-9).then(|| Self::new(self.e / n, self.n / n, self.u / n))
    }

    fn scale(self, k: f64) -> Self {
        Self::new(self.e * k, self.n * k, self.u * k)
    }

    fn add(self, o: Self) -> Self {
        Self::new(self.e + o.e, self.n + o.n, self.u + o.u)
    }

}

/// The rig and its obstacle, resolved from config and the site.
///
/// Built once; `collides` is called at every limit check, so nothing here allocates or trigs.
#[derive(Debug, Clone, Copy)]
pub struct RigModel {
    geometry: RigGeometry,
    /// The polar axis, pointing at the elevated celestial pole.
    polar: Vec3,
    /// The equator's meridian crossing — the direction at hour angle 0, declination 0. With
    /// [`WEST`] it spans the plane perpendicular to the polar axis, which is where a declination
    /// axis bearing lives. `(0, −sin φ, cos φ)` is correct in both hemispheres: below the equator
    /// the crossing is due north and high, and the signed latitude puts it there.
    equator: Vec3,
    /// The northward coordinate of the tripod's vertical centreline, in this frame.
    ///
    /// The frame's origin is the RA∩DEC crossing, but the tripod stands on the mount's
    /// *azimuth* axis, and on a real GEM head the crossing hangs `mount_axis_offset_mm` up the
    /// RA shaft from that column (the operator's: ~60 mm, ~35 mm of northing at this latitude).
    /// The offset is along the polar axis, which has no east component, so one number places
    /// the column.
    cone_axis_n: f64,
}

/// The direction at hour angle +90°, declination 0: setting due west, at every latitude.
const WEST: Vec3 = Vec3::new(-1.0, 0.0, 0.0);

/// Why a pose was refused, in the terms the operator has to act in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Collision {
    /// How far below the mount head the offending point sits, in millimetres. Negative is above.
    pub depth_mm: f64,
    /// How far past the obstacle's surface the capsule's own surface reaches, in millimetres.
    /// Strictly positive for any collision, and the number that says which of two colliding
    /// poses is *worse* — which is what lets a limit permit the motion that shrinks it
    /// (SDD §5.4.3's escape rule, added after the 2026-08-02 altitude-overshoot trap).
    pub penetration_mm: f64,
    /// What it reached.
    pub what: Obstacle,
}

/// The thing the rig would have struck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obstacle {
    /// The tripod or pier.
    Tripod,
    /// The ground.
    Ground,
}

impl RigModel {
    /// Resolve the model for a site. `None` when the operator has configured no geometry, which is
    /// the default: an unmeasured rig gets no collision limit rather than a guessed one.
    #[must_use]
    pub fn new(geometry: Option<RigGeometry>, site: Site) -> Option<Self> {
        let geometry = geometry?;
        // The polar axis is at the site's latitude, due north in the northern hemisphere and due
        // south in the southern — `s·(0, cos φ, sin φ)` gives both, since the sign flips the
        // northward component and rights the upward one at the same time.
        let phi = site.latitude_degrees.to_radians();
        let s = if site.latitude_degrees < 0.0 {
            -1.0
        } else {
            1.0
        };
        let polar = Vec3::new(0.0, s * phi.cos(), s * phi.sin());
        let equator = Vec3::new(0.0, -phi.sin(), phi.cos());
        // The tripod stands under where the head reaches its base, and the head is two moves
        // from this frame's origin: `offset` back down the RA shaft to the RA∩head-axis
        // crossing (A = −offset·p̂), then down the head's boss axis — leaning
        // `head_axis_angle_degrees` from the base plane, *against* the polar axis's rise: the
        // boss descends toward the pole side, so its foot lands poleward of the crossing
        // ("60 degrees other direction", the operator's correction of this comment's first
        // draft, 2026-08-03). The slope run is the crossing's height above the plate over the
        // tangent; a vertical boss (90°, the default) runs nowhere and this collapses to the
        // plumb line the model used before the operator measured the lean.
        let above_plate = geometry.mount_body_height_mm - geometry.mount_axis_offset_mm * polar.u;
        let theta = geometry.head_axis_angle_degrees.to_radians();
        let slope_run = above_plate * theta.cos() / theta.sin();
        let cone_axis_n =
            -geometry.mount_axis_offset_mm * polar.n + polar.n.signum() * slope_run;
        Some(Self {
            geometry,
            polar,
            equator,
            cone_axis_n,
        })
    }

    /// Whether the rig, pointing along `tube`, would be inside the tripod or the ground.
    ///
    /// `tube` is the *unit vector the telescope looks along*, in the same east/north/up frame — the
    /// altitude and azimuth the rest of the safety layer already computes. Taking the pointing
    /// direction rather than the raw axis angles is what keeps this above the HAL: the declination
    /// axis is perpendicular to both the polar axis and the optical axis, so
    ///
    /// ```text
    /// d̂ = ± (p̂ × t̂) / |p̂ × t̂|
    /// ```
    ///
    /// recovers it from two things this layer already holds, with the sign — which side of the pier
    /// the tube is on — supplied by Layer 1. No axis counters cross the HAL for this.
    ///
    /// # The pole is singular, and is treated as such
    ///
    /// When the tube points along the polar axis the cross product vanishes: every right-ascension
    /// angle produces that same pointing, so the pose genuinely does not determine where the
    /// assembly is. That is the home pose, so it is not an edge case, it is the starting pose. The
    /// model then tests **every** right-ascension angle and refuses if any of them collides, which
    /// is the only answer that cannot be wrong in the dangerous direction.
    #[must_use]
    pub fn collides(&self, tube: Horizontal, pier: Option<PierSide>) -> Option<Collision> {
        let t = tube.unit();
        let Some(dec_axis) = self.polar.cross(t).unit() else {
            // Singular: sweep the declination axis all the way round and take the worst answer.
            return self.worst_over_all_right_ascensions(t);
        };
        // The saddle is on one side of the declination axis and the counterweight on the other;
        // which one is which is the pier side. An unknown side is tested both ways, because
        // guessing it would silently halve the volume being checked.
        let signs: &[f64] = match pier {
            Some(PierSide::West) => &[1.0],
            Some(PierSide::East) => &[-1.0],
            None => &[1.0, -1.0],
        };
        signs
            .iter()
            .filter_map(|&sign| self.capsule_collides(dec_axis.scale(sign), t))
            .max_by(|a, b| a.penetration_mm.total_cmp(&b.penetration_mm))
    }

    /// Like [`collides`](Self::collides), with the declination axis's actual bearing supplied by
    /// a driver that holds mechanical state (SDD §5.4.3, the singular case resolved).
    ///
    /// `dec_axis_hour_angle_degrees` is the hour angle of the saddle direction — 0 at home,
    /// counterweight down. It is the one number the pointing cannot supply, and it dissolves both
    /// of `collides`' conservatisms at once: at the pole the model tests the pose the mount is
    /// actually in instead of every pose sharing the pointing, and everywhere else the bearing
    /// *is* the declination axis, so no pier-side sign has to be guessed or doubled.
    ///
    /// # The caller owes this a current bearing
    ///
    /// The bearing describes the mount *now*; the pointing under test is a lookahead a fraction
    /// of a second ahead. The mismatch is bounded by the lookahead window — ≲0.3° at manual
    /// rates, a few degrees at goto cruise, i.e. millimetres at this rig's lever arms — and the
    /// 2 Hz watch re-reads both on every tick, so a stale bearing cannot outlive the window that
    /// made it. Testing a *distant* target with today's bearing would be exactly the frame-mixing
    /// ADR-14 forbids; the callers of this method are the near-current lookahead paths only.
    #[must_use]
    pub fn collides_with_dec_axis(
        &self,
        tube: Horizontal,
        dec_axis_hour_angle_degrees: f64,
    ) -> Option<Collision> {
        let (sin_b, cos_b) = dec_axis_hour_angle_degrees.to_radians().sin_cos();
        let dec_axis = self.equator.scale(cos_b).add(WEST.scale(sin_b));
        self.capsule_collides(dec_axis, tube.unit())
    }

    /// The worst verdict over a full turn of the right-ascension axis (the singular case above).
    fn worst_over_all_right_ascensions(&self, tube: Vec3) -> Option<Collision> {
        // Any unit vector perpendicular to the polar axis is a candidate declination axis. Build
        // one, then rotate it about the polar axis in steps.
        let seed = self
            .polar
            .cross(Vec3::new(1.0, 0.0, 0.0))
            .unit()
            .or_else(|| self.polar.cross(Vec3::new(0.0, 1.0, 0.0)).unit())?;
        let other = self.polar.cross(seed);
        (0..SAMPLES)
            .filter_map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "i is bounded by SAMPLES = 64")]
                let angle = std::f64::consts::TAU * i as f64 / SAMPLES as f64;
                let axis = seed.scale(angle.cos()).add(other.scale(angle.sin()));
                self.capsule_collides(axis, tube)
            })
            .max_by(|a, b| a.penetration_mm.total_cmp(&b.penetration_mm))
    }

    /// Test the moving assembly — the tube capsule and, when measured, the counterweight capsule
    /// — for one declination-axis direction.
    ///
    /// `dec_axis` points from the polar axis toward the saddle, so the tube capsule's centre is
    /// `dec_axis_offset_mm` along it and its axis is the optical axis `tube`; the counterweight
    /// runs the other way, from the axes' intersection out along `-d̂`.
    fn capsule_collides(&self, dec_axis: Vec3, tube: Vec3) -> Option<Collision> {
        // The tube lies *on* the saddle, and the saddle stack — plate, dovetail, rings — builds
        // radially outward along the declination axis, so the optical axis crosses the `d̂` line
        // at `dec_axis_offset + saddle_offset` from the polar axis. The first version put the
        // saddle term perpendicular to both axes (`d̂×t̂`), which twisted the tube 230 mm
        // *sideways* and left it 230 mm closer to the pier than the metal ever is; the model then
        // refused a ~140° band of right-ascension bearings at dec-home for a tube the operator
        // could see standing half a metre clear (2026-08-02, the bearing-sweep diagnostic in
        // `examples/rig_survey.rs`). The camera confirmed the frame: the whole stack points along
        // `d̂`, away from the mount.
        let centre = dec_axis.scale(self.geometry.dec_axis_offset_mm + self.geometry.saddle_offset_mm);
        let half = self.geometry.tube_half_length_mm;
        let tube_hit = (0..=SAMPLES)
            .filter_map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "i is bounded by SAMPLES = 64")]
                let along = (2.0 * i as f64 / SAMPLES as f64 - 1.0) * half;
                self.point_collides(centre.add(tube.scale(along)), self.geometry.tube_radius_mm)
            })
            .max_by(|a, b| a.penetration_mm.total_cmp(&b.penetration_mm));
        let counterweight_hit = self.geometry.counterweight.and_then(|cw| {
            (0..=SAMPLES)
                .filter_map(|i| {
                    #[expect(clippy::cast_precision_loss, reason = "i is bounded by SAMPLES = 64")]
                    let along = cw.length_mm * i as f64 / SAMPLES as f64;
                    self.point_collides(dec_axis.scale(-along), cw.radius_mm)
                })
                .max_by(|a, b| a.penetration_mm.total_cmp(&b.penetration_mm))
        });
        // The camera stack: from the focuser's station on the optical axis, outward along the
        // saddle direction — the "parallel to the dec axis, in front of the mount" the operator
        // stated and the rig viewer confirmed by eye. A capsule of its own rather than a fatter
        // tube, because a swept ring covers 360° of orientations this stack occupies exactly one
        // of, and the difference was a third of the RA turn refused at dec-home.
        let camera_hit = self.geometry.camera.and_then(|cam| {
            let root = centre.add(tube.scale(cam.along_tube_mm));
            (0..=SAMPLES)
                .filter_map(|i| {
                    #[expect(clippy::cast_precision_loss, reason = "i is bounded by SAMPLES = 64")]
                    let along = cam.reach_mm * i as f64 / SAMPLES as f64;
                    self.point_collides(root.add(dec_axis.scale(along)), cam.radius_mm)
                })
                .max_by(|a, b| a.penetration_mm.total_cmp(&b.penetration_mm))
        });
        tube_hit
            .into_iter()
            .chain(counterweight_hit)
            .chain(camera_hit)
            .max_by(|a, b| a.penetration_mm.total_cmp(&b.penetration_mm))
    }

    /// One point of a capsule's axis against the obstacle, inflated by that capsule's radius `r`
    /// — the tube's for the tube capsule, the largest weight's for the counterweight.
    ///
    /// The origin is the intersection of the two mount axes, so `p.u` is height relative to the
    /// mount head and the ground is at `-head_height_mm`.
    ///
    /// # The tripod is a *shell*, not a solid, and this is the correction that matters
    ///
    /// The first version of this treated the truncated cone as filled, and it refused the zenith:
    /// a tube centred on the declination axis hangs its lower end inside the cone, so every pose
    /// collided. That is not what a tripod is. The volume between the legs is **free space** — it
    /// is where the counterweight hangs on every German equatorial ever built — and only the legs
    /// themselves are solid. So the obstacle is the cone's lateral *surface*, and the test is the
    /// perpendicular distance from it, taken in the (radius, height) half-plane where the surface
    /// is a straight segment.
    ///
    /// The conservatism that remains is deliberate: three thin legs are treated as a continuous
    /// skirt, so a tube that would in fact have passed through a gap is still refused. That error
    /// costs a pose; the opposite error costs a tube.
    fn point_collides(&self, p: Vec3, r: f64) -> Option<Collision> {
        let g = &self.geometry;
        let depth = -p.u;

        if p.u - r <= -g.head_height_mm {
            return Some(Collision {
                depth_mm: depth,
                penetration_mm: r - p.u - g.head_height_mm,
                what: Obstacle::Ground,
            });
        }
        // Distance to the leg surface, as a point-to-segment distance in the (ρ, z) half-plane:
        // the legs run from (top_radius, −mount_body_height) at the tripod's top plate down to
        // (base_radius, −head_height) at the foot. The top is *below* the axes by the height of
        // the mount body, which is why that is a parameter and not zero. And ρ is measured from
        // the *azimuth* axis the tripod actually stands on, not from this frame's origin — the
        // two differ by `mount_axis_offset_mm` up the RA shaft (`cone_axis_n`).
        let (px, pz) = (p.e.hypot(p.n - self.cone_axis_n), p.u);
        let (ax, az) = (g.top_radius_mm, -g.mount_body_height_mm);
        let (bx, bz) = (g.base_radius_mm, -g.head_height_mm);
        let (dx, dz) = (bx - ax, bz - az);
        let len_sq = dx.mul_add(dx, dz * dz);
        let t = if len_sq > f64::EPSILON {
            ((px - ax).mul_add(dx, (pz - az) * dz) / len_sq).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (cx, cz) = (dx.mul_add(t, ax), dz.mul_add(t, az));
        let distance = (px - cx).hypot(pz - cz);
        (distance < r).then_some(Collision {
            depth_mm: depth,
            penetration_mm: r - distance,
            what: Obstacle::Tripod,
        })
    }
}

/// A pointing direction as the rest of the safety layer produces it.
#[derive(Debug, Clone, Copy)]
pub struct Horizontal {
    /// Degrees above the horizon; negative is below it, which is a pose the mount can reach.
    pub altitude_degrees: f64,
    /// Degrees east of north.
    pub azimuth_degrees: f64,
}

impl Horizontal {
    fn unit(self) -> Vec3 {
        let (alt, az) = (
            self.altitude_degrees.to_radians(),
            self.azimuth_degrees.to_radians(),
        );
        Vec3::new(az.sin() * alt.cos(), az.cos() * alt.cos(), alt.sin())
    }
}

#[cfg(test)]
mod tests;
