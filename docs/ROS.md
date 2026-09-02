# ROS and this stack — position note

What ROS (ROS 2) is to this project: the industrial proof of our
architecture, the transfer vocabulary for robotics, an ecosystem to bridge
to, and a list of failures that are our differentiators. Not a dependency,
not a runtime, and never a kid-facing surface.

## Four things ROS uniquely gives us

**1. The existence proof for the broker's architecture.** A ROS system is a
graph of nodes exchanging typed messages over named channels — pub/sub
topics, request/reply services, long-running actions, schemas in
language-neutral `.msg`/`.srv` files with generated bindings. Structurally
that is the capability broker: channel-as-capability, typed WIT protocols.
Twenty years of industrial robots say "a robot is a graph of processes
talking over typed channels" is the right decomposition. What ROS got wrong
is exactly our thesis: its channels are *ambient* — any node can publish or
subscribe to any topic by name, discovery is global, and security
(SROS2/DDS-Security) arrived late and is rarely deployed. ROS is the broker
with the capability discipline removed: same architecture, opposite
authority model. That contrast is teachable in one sentence.

**2. The transfer target.** Like CPython for the language and Mojo at the
frontier, ROS is the vocabulary students meet if robotics goes anywhere —
FIRST alumni, college labs, industry. The play mirrors the Mojo bridge:
teach the concepts on our substrate and name their names ("this typed
channel is what ROS calls a topic; the difference is you were handed it,
you didn't discover it"). Concept transfer without installing any of it.

**3. An ecosystem to bridge to, not rebuild.** Gazebo simulation, RViz
visualization, URDF robot descriptions, tf2 transform trees, nav2, and
rosbag record/replay. With the Pi rung (PICO_BACKEND.md, "the easy
sibling") a broker↔ROS bridge is one typed channel mapped to one ROS topic
— WIT ↔ `.msg` is a natural correspondence — letting a p2w program drive a
Gazebo robot or a real ROS platform while the student touches none of the
tooling. Same shape as the Godot WIT embed: our program, their world. The
runtime side of this is already decided in MEMORY_MANAGEMENT.md
("Real-time / ROS"): p2w is the logic inside one node, the host bridges
topics through the host-import seam, values are copied at the boundary,
never shared.

**4. Its known failures are our differentiators, stated by its own
community.** Nondeterminism is ROS's notorious pain — message ordering and
timing make bugs unreproducible, and rosbag replay is approximate. Our
deterministic story (seeded xorshift on every surface, exact replay,
channel-scoped authority) is the thing ROS users wish they had. And ROS
tooling is famously unteachable below university level. ROS validates the
problem domain and leaves both seats we want — K-12 and
verified/deterministic — empty.

## Positions

- **ROS as middleware on our targets: no.** micro-ROS runs on RP2350-class
  boards, so "just put ROS on the Pico" will recur. It is the same tension
  as wasmi-on-Pico: middleware where our position is compiled-and-bare, and
  it imports the ambient-authority model onto the one target where we
  control every symbol. Rejected; revisit only if a partner robot ships
  with it burned in.
- **ROS as a host the broker bridges to: yes, when a consumer exists.** The
  Pi rung is the natural home (ROS 2 runs there; the Pico stays
  bridge-free). Gate: an actual classroom robot or Gazebo lesson that needs
  it — no speculative bridge code.
- **ROS names in the curriculum: yes.** One glossary line per concept
  (topic, node, bag, transform) at the point our equivalent is taught. The
  claim "what you learned transfers to ROS" must stay checkable — if we say
  it, a lesson shows it.

## What NOT to take

No DDS, no colcon/launch-file tooling shape, no dynamic discovery, no
global namespace. Each is either the ambient-authority model or the
complexity that makes ROS unteachable — the two things the stack exists to
avoid.
