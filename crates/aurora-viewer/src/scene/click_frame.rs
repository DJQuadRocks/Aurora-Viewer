//! Object poses at the moment of a click.
//!
//! A click keeps the ray and the depth of the frame it was made on and
//! runs again when that depth comes back, a few frames later
//! (`app/click_pick.rs`). The search used to test the objects where they
//! are *then*. On a moving vehicle the switches have left that ray (about
//! a metre in three frames at 20 m/s), so the click misses (#4088, #3312).
//!
//! `LLPickInfo::fetchResults` (indra/newview/llviewerwindow.cpp, originally
//! LGPL 2.1) resolves the pick with the camera and the objects of one
//! moment, so a vehicle and the camera that rides it move together. The
//! poses here are that moment: the frame of the click, the same one as the
//! ray and the depth. Searching the objects where they are now, along a ray
//! moved with the seat, is not the same. A ray carried forward can meet a
//! stationary prim the click never touched, and a prim moving on its own
//! (the camera still) is not carried by the seat.

use glam::{Quat, Vec3};
use std::collections::HashMap;
use uuid::Uuid;

/// Where one object was drawn when the click was made.
#[derive(Clone, Copy, Debug)]
pub struct Pose {
    pub id: Uuid,
    pub pos: Vec3,
    pub rot: Quat,
    pub scale: Vec3,
    /// Bounding sphere, the broad phase of `ray_may_hit`.
    pub center: Vec3,
    pub radius: f32,
}

/// The objects a click could meet, placed as they were when it was made.
#[derive(Clone, Debug, Default)]
pub struct ClickFrame {
    /// Pick-index order. A media prim the index did not list is kept in
    /// `poses` only (`keep`), so its face test can still use the click.
    order: Vec<usize>,
    poses: HashMap<usize, Pose>,
}

impl ClickFrame {
    pub fn from_indices(indices: impl IntoIterator<Item = usize>, mut pose_of: impl FnMut(usize) -> Option<Pose>) -> Self {
        let mut order = Vec::new();
        let mut poses = HashMap::new();
        for idx in indices {
            if poses.contains_key(&idx) {
                continue;
            }
            if let Some(pose) = pose_of(idx) {
                poses.insert(idx, pose);
                order.push(idx);
            }
        }
        Self { order, poses }
    }

    /// The pose, when `idx` is still the object that was clicked.
    pub fn get(&self, idx: usize, id: Uuid) -> Option<&Pose> {
        self.poses.get(&idx).filter(|pose| pose.id == id)
    }

    /// A pose for a media prim the pick index did not list. It is not a
    /// candidate of the prim search: only its face test reads it.
    pub fn keep(&mut self, idx: usize, pose: Pose) {
        self.poses.entry(idx).or_insert(pose);
    }

    /// Candidates whose slot still holds the object from the click.
    pub fn copy_live(&self, id_of: impl Fn(usize) -> Option<Uuid>, out: &mut Vec<usize>) {
        out.clear();
        out.extend(
            self.order
                .iter()
                .copied()
                .filter(|&idx| id_of(idx).is_some_and(|id| self.get(idx, id).is_some())),
        );
    }
}

/// Whether a sphere can meet a ray. The same test as `picking::ray_may_hit`:
/// rigged meshes and objects without a radius always pass; otherwise the
/// sphere may sit 5 % + 5 cm off the ray.
pub fn sphere_meets_ray(center: Vec3, radius: f32, rigged: bool, ray: (Vec3, Vec3)) -> bool {
    if rigged || radius <= 0.0 {
        return true;
    }
    let r = radius * 1.05 + 0.05;
    let to = center - ray.0;
    let along = to.dot(ray.1);
    if along < -r {
        return false;
    }
    to.length_squared() - along * along <= r * r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose(id: Uuid, center: Vec3) -> Pose {
        Pose {
            id,
            pos: center,
            rot: Quat::IDENTITY,
            scale: Vec3::ONE,
            center,
            radius: 0.2,
        }
    }

    #[test]
    fn a_replaced_slot_does_not_keep_the_click_pose() {
        let id = Uuid::from_u128(7);
        let frame = ClickFrame::from_indices([3], |_| Some(pose(id, Vec3::X)));
        assert!(frame.get(3, id).is_some());
        assert!(frame.get(3, Uuid::from_u128(8)).is_none());
        let mut live = Vec::new();
        frame.copy_live(|_| Some(id), &mut live);
        assert_eq!(live, vec![3]);
        frame.copy_live(|_| Some(Uuid::from_u128(8)), &mut live);
        assert!(live.is_empty(), "the slot now holds another object");
    }

    /// Ray against an axis-aligned box. `dir` must be a unit vector.
    fn aabb_hit(center: Vec3, half: Vec3, origin: Vec3, dir: Vec3) -> bool {
        let mut t0 = 0.0f32;
        let mut t1 = f32::MAX;
        for axis in 0..3 {
            let o = origin[axis];
            let d = dir[axis];
            let min = center[axis] - half[axis];
            let max = center[axis] + half[axis];
            if d.abs() < 1e-8 {
                if o < min || o > max {
                    return false;
                }
                continue;
            }
            let inv = 1.0 / d;
            let (mut a, mut b) = ((min - o) * inv, (max - o) * inv);
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            t0 = t0.max(a);
            t1 = t1.min(b);
            if t0 > t1 {
                return false;
            }
        }
        t1 >= 0.0 && t0 < f32::MAX
    }

    #[test]
    fn the_click_keeps_its_ray_and_a_moved_ray_can_hit_scenery() {
        // A switch ahead and a little aside. The ray goes through it.
        let switch = Vec3::new(2.0, 0.4, 0.0);
        let half = Vec3::splat(0.05);
        let dir = switch.normalize();
        let origin = Vec3::ZERO;
        assert!(aabb_hit(switch, half, origin, dir), "the click meets the switch");

        // The vehicle, and the camera on it, move a metre forward.
        let moved = switch + Vec3::X;
        assert!(!aabb_hit(moved, half, origin, dir), "the old ray misses the switch where it is now");
        let carried = origin + Vec3::X;
        assert!(
            aabb_hit(moved, half, carried, dir),
            "a ray carried with the vehicle still meets the switch"
        );

        // A post that was beside the click. Carrying the ray forward puts
        // the post on it, in front of the switch. The click did not touch it.
        let post = carried + dir * 0.5;
        assert!(aabb_hit(post, half, carried, dir), "the carried ray meets the post");
        assert!(!aabb_hit(post, half, origin, dir), "the click's own ray does not");

        // Standing still, another object slides off the ray. Carrying
        // nothing (no seat motion) leaves the miss. The pose from the click
        // is what the ray hit.
        let slid = switch + Vec3::Y;
        assert!(!aabb_hit(slid, half, origin, dir));
        assert!(aabb_hit(switch, half, origin, dir));
    }

    #[test]
    fn the_broad_phase_uses_the_sphere_from_the_click() {
        let ray = (Vec3::ZERO, Vec3::X);
        assert!(sphere_meets_ray(Vec3::new(2.0, 0.0, 0.0), 0.2, false, ray));
        assert!(!sphere_meets_ray(Vec3::new(2.0, 3.0, 0.0), 0.2, false, ray));
        assert!(sphere_meets_ray(Vec3::new(2.0, 3.0, 0.0), 0.0, false, ray));
        assert!(sphere_meets_ray(Vec3::new(2.0, 3.0, 0.0), 0.2, true, ray));
    }
}
