//! The robot arena simulator (docs/ROBOT.md, step 2): a flat world of
//! differential-drive robots on a sumo ring, advanced in fixed ticks.
//!
//! It is a host for the `robot` module, like the board's firmware: student
//! code asks it to drive and to read sensors. Everything is deterministic —
//! no wall clock, no randomness — so a match replays exactly and tests run
//! whole matches without a browser. Units are millimetres, milliseconds and
//! radians; angles turn anticlockwise from the +x axis.
//!
//! Game-engine shape on purpose: a fixed physics tick separate from drawing,
//! entities with a position and heading, and events instead of polling.

use std::f64::consts::PI;

/// One physics tick. Drawing may run at any rate; physics never varies.
pub const TICK_MS: u64 = 10;

/// How a robot is built. One standard class design for now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Build {
    /// Collision radius: a 10 cm robot treated as a circle.
    pub radius_mm: f64,
    /// Distance between the two drive wheels.
    pub wheelbase_mm: f64,
    /// Wheel surface speed at 100% power: an N20 at 300 rpm on a 30 mm
    /// wheel is about 470 mm/s.
    pub top_speed_mm_s: f64,
    /// Line sensors, as (forward, left) offsets from the centre.
    pub line_sensors: [(f64, f64); 2],
}

impl Build {
    pub const STANDARD: Build = Build {
        radius_mm: 50.0,
        wheelbase_mm: 80.0,
        top_speed_mm_s: 470.0,
        line_sensors: [(45.0, 30.0), (45.0, -30.0)],
    };
}

/// The rules student code cannot override. Every number is a setting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rules {
    /// Motor power cap, percent: a full 2S battery on 6 V motors.
    pub power_cap_pct: f64,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            power_cap_pct: 70.0,
        }
    }
}

/// A sumo ring: everything within `radius_mm` of the origin is floor, and
/// the outer `edge_mm` of it is the white border line sensors look for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ring {
    pub radius_mm: f64,
    pub edge_mm: f64,
}

impl Ring {
    /// The 77 cm Mini sumo ring with its 2.5 cm white edge.
    pub const MINI: Ring = Ring {
        radius_mm: 385.0,
        edge_mm: 25.0,
    };
}

/// What happened, and when. Games score from these.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A robot's centre left the ring.
    LeftRing { robot: usize, at_ms: u64 },
}

/// How a robot without a student program drives itself. Built-in practice
/// opponents, until a game can give every robot its own program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Behaviour {
    /// Driven only by `set_motor` (a student's robot, or one standing still).
    #[default]
    Driven,
    /// Turns on the spot: a moving target to aim at.
    Spin,
    /// Turns toward the nearest robot and drives at it.
    Charge,
}

impl Behaviour {
    /// Every behaviour, with a name a menu can show.
    pub const ALL: [(Behaviour, &'static str); 3] = [
        (Behaviour::Driven, "still"),
        (Behaviour::Spin, "spinning"),
        (Behaviour::Charge, "charging"),
    ];
}

#[derive(Debug, Clone, PartialEq)]
pub struct Robot {
    pub name: String,
    pub x_mm: f64,
    pub y_mm: f64,
    pub heading: f64,
    /// Motor commands as the student gave them, -100..100.
    pub command: (f64, f64),
    pub out_of_ring: bool,
    pub behaviour: Behaviour,
}

/// A request the simulator cannot satisfy, worded for the student.
#[derive(Debug, Clone, PartialEq)]
pub struct SimError(pub String);

#[derive(Debug, Clone)]
pub struct Arena {
    pub build: Build,
    pub rules: Rules,
    pub ring: Ring,
    pub robots: Vec<Robot>,
    pub now_ms: u64,
    pub events: Vec<Event>,
    /// How far the distance sensor can see; also its "nothing there" value,
    /// matching the robot module's stand-in.
    pub sensor_range_mm: f64,
}

impl Arena {
    pub fn new(ring: Ring) -> Arena {
        Arena {
            build: Build::STANDARD,
            rules: Rules::default(),
            ring,
            robots: Vec::new(),
            now_ms: 0,
            events: Vec::new(),
            sensor_range_mm: 2000.0,
        }
    }

    /// Put a robot in the arena; returns its id. `heading_deg` turns
    /// anticlockwise from facing +x.
    pub fn spawn(&mut self, name: &str, x_mm: f64, y_mm: f64, heading_deg: f64) -> usize {
        self.robots.push(Robot {
            name: name.to_string(),
            x_mm,
            y_mm,
            heading: heading_deg.to_radians(),
            command: (0.0, 0.0),
            out_of_ring: false,
            behaviour: Behaviour::Driven,
        });
        self.robots.len() - 1
    }

    fn robot(&self, id: usize) -> Result<&Robot, SimError> {
        self.robots
            .get(id)
            .ok_or_else(|| SimError(format!("there's no robot {id} in this arena")))
    }

    /// Choose how robot `id` drives itself. Switching to `Driven` stops it.
    pub fn set_behaviour(&mut self, id: usize, behaviour: Behaviour) -> Result<(), SimError> {
        self.robot(id)?;
        self.robots[id].behaviour = behaviour;
        if behaviour == Behaviour::Driven {
            self.robots[id].command = (0.0, 0.0);
        }
        Ok(())
    }

    /// Set the commands of every self-driving robot from what it sees now.
    /// Robots out of the ring stop: their round is over.
    fn drive_behaviours(&mut self) {
        for id in 0..self.robots.len() {
            let r = &self.robots[id];
            if r.out_of_ring {
                if r.behaviour != Behaviour::Driven {
                    self.robots[id].command = (0.0, 0.0);
                }
                continue;
            }
            let command = match r.behaviour {
                Behaviour::Driven => continue,
                Behaviour::Spin => (-40.0, 40.0),
                Behaviour::Charge => match self.nearest(id) {
                    None => (0.0, 0.0),
                    Some(target) => {
                        let t = &self.robots[target];
                        let bearing = (t.y_mm - r.y_mm).atan2(t.x_mm - r.x_mm);
                        let error = wrap(bearing - r.heading);
                        // Turn harder the further off it points; full ahead
                        // once roughly lined up.
                        let turn = (error / (PI / 4.0)).clamp(-1.0, 1.0) * 60.0;
                        let ahead = if error.abs() < PI / 6.0 { 100.0 } else { 0.0 };
                        (ahead - turn, ahead + turn)
                    }
                },
            };
            self.robots[id].command = (
                command.0.clamp(-100.0, 100.0),
                command.1.clamp(-100.0, 100.0),
            );
        }
    }

    /// The closest other robot still in the ring.
    fn nearest(&self, id: usize) -> Option<usize> {
        let me = &self.robots[id];
        self.robots
            .iter()
            .enumerate()
            .filter(|(i, r)| *i != id && !r.out_of_ring)
            .min_by(|(_, a), (_, b)| {
                let da = (a.x_mm - me.x_mm).hypot(a.y_mm - me.y_mm);
                let db = (b.x_mm - me.x_mm).hypot(b.y_mm - me.y_mm);
                da.total_cmp(&db)
            })
            .map(|(i, _)| i)
    }

    // --- the robot module's calls -------------------------------------

    /// `robot.set_motor(left, right)`. Values outside -100..100 are held at
    /// the limit, as a real speed controller would.
    pub fn set_motor(&mut self, id: usize, left: f64, right: f64) -> Result<(), SimError> {
        self.robot(id)?;
        let clamp = |v: f64| v.clamp(-100.0, 100.0);
        self.robots[id].command = (clamp(left), clamp(right));
        Ok(())
    }

    /// `robot.read_distance(n)`: millimetres from the robot's front to the
    /// nearest other robot straight ahead, or the sensor's range if none.
    pub fn read_distance(&self, id: usize, sensor: i64) -> Result<f64, SimError> {
        let me = self.robot(id)?;
        if sensor != 0 {
            return Err(SimError(format!(
                "this robot has one distance sensor, number 0, not {sensor}"
            )));
        }
        let (dx, dy) = (me.heading.cos(), me.heading.sin());
        let r = self.build.radius_mm;
        // The sensor sits on the front edge, looking forward.
        let (sx, sy) = (me.x_mm + dx * r, me.y_mm + dy * r);
        let mut best = self.sensor_range_mm;
        for (other_id, other) in self.robots.iter().enumerate() {
            if other_id == id {
                continue;
            }
            if let Some(d) = ray_circle(sx, sy, dx, dy, other.x_mm, other.y_mm, r) {
                best = best.min(d);
            }
        }
        Ok(best.round())
    }

    /// `robot.read_line(n)`: whether line sensor `n` is over the white edge
    /// or past it.
    pub fn read_line(&self, id: usize, sensor: i64) -> Result<bool, SimError> {
        let me = self.robot(id)?;
        let Some(&(fwd, left)) = usize::try_from(sensor)
            .ok()
            .and_then(|i| self.build.line_sensors.get(i))
        else {
            return Err(SimError(format!(
                "this robot has line sensors 0 and 1, not {sensor}"
            )));
        };
        let (c, s) = (me.heading.cos(), me.heading.sin());
        let px = me.x_mm + fwd * c - left * s;
        let py = me.y_mm + fwd * s + left * c;
        Ok(px.hypot(py) >= self.ring.radius_mm - self.ring.edge_mm)
    }

    /// `robot.tilt()`: a flat world never tips, so robots read level until a
    /// game sets otherwise.
    pub fn tilt(&self, id: usize) -> Result<f64, SimError> {
        self.robot(id).map(|_| 0.0)
    }

    // --- physics -------------------------------------------------------

    /// Advance one tick: the arbiter limits each command, every robot drives
    /// as a differential pair, contacts are resolved as pushes, then rule
    /// events are recorded.
    pub fn tick(&mut self) {
        self.drive_behaviours();
        let dt = TICK_MS as f64 / 1000.0;
        let cap = self.rules.power_cap_pct / 100.0;
        let mut velocity = Vec::with_capacity(self.robots.len());
        for r in &mut self.robots {
            let to_speed = |pct: f64| pct / 100.0 * cap * self.build.top_speed_mm_s;
            let (vl, vr) = (to_speed(r.command.0), to_speed(r.command.1));
            let v = (vl + vr) / 2.0;
            let w = (vr - vl) / self.build.wheelbase_mm;
            // Integrate along the arc at the midpoint heading: exact enough
            // at 10 ms and stable for spins on the spot.
            let mid = r.heading + w * dt / 2.0;
            r.x_mm += v * mid.cos() * dt;
            r.y_mm += v * mid.sin() * dt;
            r.heading = wrap(r.heading + w * dt);
            velocity.push((v * mid.cos(), v * mid.sin()));
        }
        self.resolve_contacts(&velocity);
        self.now_ms += TICK_MS;
        for (id, r) in self.robots.iter_mut().enumerate() {
            if !r.out_of_ring && r.x_mm.hypot(r.y_mm) > self.ring.radius_mm {
                r.out_of_ring = true;
                self.events.push(Event::LeftRing {
                    robot: id,
                    at_ms: self.now_ms,
                });
            }
        }
    }

    /// Resolve contacts as pushes. Each side gives way in proportion to how
    /// hard the OTHER drives into it: a robot driving at a stationary one
    /// shoves it, equal pushes stall, unequal pushes move both the stronger
    /// robot's way.
    ///
    /// A pushed robot moves in the direction its pusher drives, not along
    /// the line between centres. Wheel grip is what stops a real robot from
    /// sliding sideways; without it two circles pushing head-on are an
    /// unstable balance, and an offset of a millimetre grows until they slip
    /// past each other in about a second. A glancing contact falls back to
    /// the centre line, and a final exact separation guarantees no overlap.
    /// Equal mass, no rotation from off-centre hits yet.
    fn resolve_contacts(&mut self, velocity: &[(f64, f64)]) {
        let touch = 2.0 * self.build.radius_mm;
        for i in 0..self.robots.len() {
            for j in i + 1..self.robots.len() {
                let Some((nx, ny, overlap)) = self.overlap(i, j, touch) else {
                    continue;
                };
                let a_push = (velocity[i].0 * nx + velocity[i].1 * ny).max(0.0);
                let b_push = (-(velocity[j].0 * nx + velocity[j].1 * ny)).max(0.0);
                let total = a_push + b_push;
                let (b_share, a_share) = if total > 1e-9 {
                    (a_push / total, b_push / total)
                } else {
                    (0.5, 0.5)
                };
                // j moves the way i drives; i moves the way j drives.
                let (bx, by) = shove_direction(velocity[i], (nx, ny));
                let (ax, ay) = shove_direction(velocity[j], (-nx, -ny));
                let b_dist = overlap * b_share / (bx * nx + by * ny);
                let a_dist = overlap * a_share / -(ax * nx + ay * ny);
                self.robots[j].x_mm += bx * b_dist;
                self.robots[j].y_mm += by * b_dist;
                self.robots[i].x_mm += ax * a_dist;
                self.robots[i].y_mm += ay * a_dist;
                // The directed moves are linear in a curved contact; clear
                // whatever overlap is left, evenly, along the centre line.
                if let Some((nx, ny, rest)) = self.overlap(i, j, touch) {
                    self.robots[j].x_mm += nx * rest / 2.0;
                    self.robots[j].y_mm += ny * rest / 2.0;
                    self.robots[i].x_mm -= nx * rest / 2.0;
                    self.robots[i].y_mm -= ny * rest / 2.0;
                }
            }
        }
    }

    /// Unit vector from robot i to robot j and how far they overlap, if they do.
    fn overlap(&self, i: usize, j: usize, touch: f64) -> Option<(f64, f64, f64)> {
        let (ax, ay) = (self.robots[i].x_mm, self.robots[i].y_mm);
        let (bx, by) = (self.robots[j].x_mm, self.robots[j].y_mm);
        let dist = (bx - ax).hypot(by - ay);
        if dist >= touch {
            return None;
        }
        // Coincident centres: separate along +x rather than divide by zero.
        let (nx, ny) = if dist > 1e-9 {
            ((bx - ax) / dist, (by - ay) / dist)
        } else {
            (1.0, 0.0)
        };
        Some((nx, ny, touch - dist))
    }

    /// Advance whole ticks covering at least `ms` milliseconds.
    pub fn run_for(&mut self, ms: u64) {
        let end = self.now_ms + ms;
        while self.now_ms < end {
            self.tick();
        }
    }
}

/// Which way a robot gets shoved: along its pusher's motion when that
/// motion points well into the contact, else straight along the contact
/// normal `n` (a glancing touch, or a pusher that isn't moving).
fn shove_direction(pusher_velocity: (f64, f64), n: (f64, f64)) -> (f64, f64) {
    let (vx, vy) = pusher_velocity;
    let speed = vx.hypot(vy);
    if speed > 1e-9 {
        let (ux, uy) = (vx / speed, vy / speed);
        if ux * n.0 + uy * n.1 >= 0.5 {
            return (ux, uy);
        }
    }
    n
}

/// `every(ms, handler)` timers on simulator time. Pure bookkeeping: the
/// caller asks which handlers are due and runs them, so a host never holds
/// a borrow of this while student code (which may add timers) runs.
#[derive(Debug, Clone, Default)]
pub struct Timers {
    /// (period, next due time, handler id), in registration order.
    entries: Vec<(u64, u64, i32)>,
}

impl Timers {
    /// The shortest period a program may ask for, per docs/ROBOT.md.
    pub const MIN_PERIOD_MS: u64 = 10;

    /// Register a handler to run every `ms`, first at `now_ms + ms`.
    pub fn add(&mut self, ms: i64, id: i32, now_ms: u64) {
        let period = u64::try_from(ms).unwrap_or(0).max(Self::MIN_PERIOD_MS);
        self.entries.push((period, now_ms + period, id));
    }

    /// Take the next handler due at `now_ms`, earliest first and then in
    /// registration order, and reschedule it. Call until `None`.
    pub fn next_due(&mut self, now_ms: u64) -> Option<i32> {
        let index = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.1 <= now_ms)
            .min_by_key(|(i, e)| (e.1, *i))
            .map(|(i, _)| i)?;
        let entry = &mut self.entries[index];
        entry.1 += entry.0;
        Some(entry.2)
    }
}

/// Keep an angle in (-PI, PI].
fn wrap(a: f64) -> f64 {
    let mut a = a % (2.0 * PI);
    if a <= -PI {
        a += 2.0 * PI;
    } else if a > PI {
        a -= 2.0 * PI;
    }
    a
}

/// Distance along a unit ray from (sx, sy) to the first point of a circle,
/// if the ray meets it ahead of the start.
fn ray_circle(sx: f64, sy: f64, dx: f64, dy: f64, cx: f64, cy: f64, r: f64) -> Option<f64> {
    let (fx, fy) = (sx - cx, sy - cy);
    let b = fx * dx + fy * dy;
    let c = fx * fx + fy * fy - r * r;
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let t = -b - disc.sqrt();
    (t >= 0.0).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn full_power_is_capped_by_the_arbiter() {
        let mut a = Arena::new(Ring::MINI);
        let r = a.spawn("red", 0.0, 0.0, 0.0);
        a.set_motor(r, 100.0, 100.0).unwrap();
        a.run_for(1000);
        // 70% of 470 mm/s for one second, straight along +x.
        assert!(close(a.robots[r].x_mm, 329.0, 0.5), "{:?}", a.robots[r]);
        assert!(close(a.robots[r].y_mm, 0.0, 1e-9));
    }

    #[test]
    fn opposite_wheels_spin_on_the_spot() {
        let mut a = Arena::new(Ring::MINI);
        let r = a.spawn("red", 0.0, 0.0, 0.0);
        a.set_motor(r, -50.0, 50.0).unwrap();
        a.run_for(500);
        let me = &a.robots[r];
        assert!(close(me.x_mm, 0.0, 1e-9) && close(me.y_mm, 0.0, 1e-9));
        // w = (164.5 - -164.5) / 80 rad/s for 0.5 s, anticlockwise, wrapped.
        let expected = wrap(2.0 * 164.5 / 80.0 * 0.5);
        assert!(close(me.heading, expected, 1e-9), "{}", me.heading);
    }

    #[test]
    fn commands_are_held_to_the_limit() {
        let mut a = Arena::new(Ring::MINI);
        let r = a.spawn("red", 0.0, 0.0, 0.0);
        a.set_motor(r, 250.0, -900.0).unwrap();
        assert_eq!(a.robots[r].command, (100.0, -100.0));
    }

    #[test]
    fn distance_sees_a_robot_ahead_and_nothing_behind() {
        let mut a = Arena::new(Ring::MINI);
        let red = a.spawn("red", -150.0, 0.0, 0.0);
        let blue = a.spawn("blue", 150.0, 0.0, 180.0);
        // Centres 300 apart, two 50 mm radii between them.
        assert_eq!(a.read_distance(red, 0).unwrap(), 200.0);
        assert_eq!(a.read_distance(blue, 0).unwrap(), 200.0);
        a.robots[red].heading = PI;
        assert_eq!(a.read_distance(red, 0).unwrap(), 2000.0);
        assert!(a.read_distance(red, 1).unwrap_err().0.contains("number 0"));
    }

    #[test]
    fn line_sensors_find_the_white_edge() {
        let mut a = Arena::new(Ring::MINI);
        let r = a.spawn("red", 0.0, 0.0, 0.0);
        assert!(!a.read_line(r, 0).unwrap() && !a.read_line(r, 1).unwrap());
        // Sensors sit 45 mm ahead: at x = 320 they reach 365 > 360.
        a.robots[r].x_mm = 320.0;
        assert!(a.read_line(r, 0).unwrap() && a.read_line(r, 1).unwrap());
        // Facing along the edge, only the outer sensor sees it.
        a.robots[r].heading = PI / 2.0;
        a.robots[r].x_mm = 330.0;
        assert!(!a.read_line(r, 0).unwrap(), "left sensor is inboard");
        assert!(a.read_line(r, 1).unwrap(), "right sensor is outboard");
        assert!(a.read_line(r, 2).is_err());
    }

    #[test]
    fn driving_off_the_ring_is_an_event_once() {
        let mut a = Arena::new(Ring::MINI);
        let r = a.spawn("red", 0.0, 0.0, 0.0);
        a.set_motor(r, 100.0, 100.0).unwrap();
        a.run_for(3000);
        assert_eq!(a.events.len(), 1, "{:?}", a.events);
        let Event::LeftRing { robot, at_ms } = a.events[0].clone();
        assert_eq!(robot, r);
        // 385 mm at 329 mm/s is a little over 1.17 s.
        assert!((1170..=1180).contains(&at_ms), "{at_ms}");
    }

    fn sumo(left_power: f64, right_power: f64) -> Arena {
        let mut a = Arena::new(Ring::MINI);
        let red = a.spawn("red", -100.0, 0.0, 0.0);
        let blue = a.spawn("blue", 100.0, 0.0, 180.0);
        a.set_motor(red, left_power, left_power).unwrap();
        a.set_motor(blue, right_power, right_power).unwrap();
        a
    }

    #[test]
    fn robots_never_pass_through_each_other() {
        let mut a = sumo(100.0, 100.0);
        for _ in 0..300 {
            a.tick();
            let (r, b) = (&a.robots[0], &a.robots[1]);
            let gap = (b.x_mm - r.x_mm).hypot(b.y_mm - r.y_mm);
            assert!(gap >= 100.0 - 1e-6, "overlap at {} ms: {gap}", a.now_ms);
        }
    }

    #[test]
    fn equal_pushes_stall_in_the_middle() {
        let mut a = sumo(100.0, 100.0);
        a.run_for(3000);
        assert!(close(a.robots[0].x_mm, -50.0, 1e-6), "{:?}", a.robots[0]);
        assert!(close(a.robots[1].x_mm, 50.0, 1e-6), "{:?}", a.robots[1]);
        assert!(a.events.is_empty());
    }

    #[test]
    fn the_stronger_push_wins_the_ring() {
        let mut a = sumo(100.0, 50.0);
        // A round ends at the first ring-out; run until there is one.
        while a.events.is_empty() && a.now_ms < 10_000 {
            a.tick();
        }
        let Event::LeftRing { robot, .. } = a.events[0].clone();
        assert_eq!(robot, 1, "blue should be pushed out: {:?}", a.robots);
        assert!(!a.robots[0].out_of_ring, "{:?}", a.robots[0]);
    }

    #[test]
    fn an_offset_head_on_push_stays_head_on() {
        // A millimetre off-centre. Pure centre-line separation slid these
        // past each other within about a second.
        let mut a = Arena::new(Ring {
            radius_mm: 5000.0,
            edge_mm: 25.0,
        });
        let red = a.spawn("red", -100.0, 0.0, 0.0);
        let blue = a.spawn("blue", 100.0, 1.0, 180.0);
        a.set_motor(red, 100.0, 100.0).unwrap();
        a.set_motor(blue, 50.0, 50.0).unwrap();
        a.run_for(8000);
        let (r, b) = (&a.robots[0], &a.robots[1]);
        assert!(b.x_mm > r.x_mm, "they slid past: {r:?} {b:?}");
        assert!((b.y_mm - r.y_mm).abs() < 5.0, "{r:?} {b:?}");
    }

    #[test]
    fn a_robot_that_does_not_push_is_shoved() {
        let mut a = sumo(100.0, 0.0);
        a.run_for(500);
        // Contact at once (gap 0 after 0.304 s); blue rides ahead of red.
        let gap = a.robots[1].x_mm - a.robots[0].x_mm;
        assert!(close(gap, 100.0, 1e-6), "{gap}");
        assert!(a.robots[1].x_mm > 100.0, "{:?}", a.robots[1]);
    }

    #[test]
    fn timers_fire_on_simulator_time_in_a_fixed_order() {
        let mut t = Timers::default();
        t.add(50, 7, 0);
        t.add(20, 8, 0);
        t.add(3, 9, 0); // below the minimum: runs every 10 ms
        let mut fired = Vec::new();
        for now in (0..=60).step_by(10) {
            while let Some(id) = t.next_due(now) {
                fired.push((now, id));
            }
        }
        assert_eq!(
            fired,
            vec![
                (10, 9),
                (20, 8),
                (20, 9),
                (30, 9),
                (40, 8),
                (40, 9),
                (50, 7),
                (50, 9),
                (60, 8),
                (60, 9),
            ]
        );
    }

    #[test]
    fn a_charging_opponent_finds_and_pushes_a_still_robot_out() {
        let mut a = Arena::new(Ring::MINI);
        // The student's robot sits still; the opponent starts facing away.
        let you = a.spawn("you", -150.0, 50.0, 0.0);
        let them = a.spawn("practice", 150.0, -50.0, 0.0);
        a.set_behaviour(them, Behaviour::Charge).unwrap();
        while a.events.is_empty() && a.now_ms < 10_000 {
            a.tick();
        }
        let Event::LeftRing { robot, .. } = a.events[0].clone();
        assert_eq!(robot, you, "{:?}", a.robots);
        assert!(!a.robots[them].out_of_ring);
    }

    #[test]
    fn a_spinning_opponent_stays_put_and_stops_when_set_still() {
        let mut a = Arena::new(Ring::MINI);
        let them = a.spawn("practice", 100.0, 0.0, 0.0);
        a.set_behaviour(them, Behaviour::Spin).unwrap();
        a.run_for(1000);
        let r = &a.robots[them];
        assert!(
            close(r.x_mm, 100.0, 1e-6) && close(r.y_mm, 0.0, 1e-6),
            "{r:?}"
        );
        assert!(r.heading.abs() > 0.1, "it should have turned: {r:?}");
        a.set_behaviour(them, Behaviour::Driven).unwrap();
        let before = a.robots[them].heading;
        a.run_for(500);
        assert_eq!(a.robots[them].heading, before);
    }

    #[test]
    fn a_run_replays_exactly() {
        let play = || {
            let mut a = Arena::new(Ring::MINI);
            let r = a.spawn("red", 0.0, 0.0, 30.0);
            a.set_motor(r, 80.0, 35.0).unwrap();
            a.run_for(2000);
            a.robots[r].clone()
        };
        assert_eq!(play(), play());
    }
}
