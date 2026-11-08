# Monitor Grid Design

## Intent

Let a user arrange physical displays from the local computer and an authenticated remote computer in one logical grid. Crossing an edge between displays owned by different computers switches input to that computer. This is for a Windows machine connected to Linux with Hyprland, including different monitor counts on the two machines.

## Goals

- Show one cell per physical display in a GTK editor. Local displays keep their detected OS topology; remote display cells can be positioned individually in the grid.
- Discover Windows displays and Hyprland outputs and distinguish them by stable, per-machine identifiers.
- Use grid adjacency to select a remote monitor and the edge where input enters it. Cell sizes are logical and equal; no pixel coordinate or edge-offset mapping is needed.
- Keep native movement between displays on the same computer under the operating system.
- Persist the grid in TOML and synchronize the layout with the authenticated peer so either side can return control through the same arrangement.
- Retain the existing four-direction behavior with peers that do not advertise monitor-grid support.

## Non-goals

- Reproducing remote monitor resolutions, scaling, or pixel-aligned cursor positions.
- Changing how keyboard, clipboard, or file transfer work.
- Adding monitor-grid support to macOS or capture backends other than Windows and Hyprland in this implementation.
- Building a virtual display driver or changing the peer authentication model.

## Constraints

- Today `input_capture` reports only `Position::{Left,Right,Top,Bottom}`. Windows has display rectangles but discards the display identity; the Hyprland layer-shell backend has output names, positions, and sizes but emits only a direction.
- Client configuration and GTK currently assign one direction per remote client. The wire protocol's `Enter` event also carries only that direction.
- Protocol decoding uses a fixed-size event buffer and skips unknown event types. New topology and monitor-entry messages must be bounded and must not make old peers disconnect.
- Windows display identifiers and Hyprland output names are OS-specific. The shared routing layer must use an opaque monitor ID paired with the authenticated machine identity.
- Hyprland capture currently groups edges by the four global directions and suppresses edges shared by two local outputs. The new capture event must preserve the output name so each edge can select its own remote neighbor.

## Approaches

1. **Physical-monitor grid (selected).** Represent every monitor as a cell and route only cross-machine adjacency. This directly supports N monitors per device and matches the requested setup. It requires monitor-aware capture, a topology exchange, a monitor-aware entry event, and a grid editor.
2. **Device grid.** Represent each computer as one block and keep all of its monitors under native OS navigation. This preserves the current wire protocol and is simpler, but does not let the user place individual remote monitors, so it does not meet the requested N-monitor mapping.

The user selected the physical-monitor grid.

## Behavior and implementation choices

- The grid stores integer cell coordinates, not pixel coordinates. Each cell is one display regardless of resolution. Occupied cells are unique; empty cells are allowed. The local display cluster is initialized from native output positions and stays fixed relative to itself; users arrange remote outputs around it.
- Example: `Linux DP-1` and `Linux HDMI-A-1` can occupy cells above Windows `DISPLAY1`, `DISPLAY2`, and `DISPLAY3`. Only a cell edge that neighbors a display on another machine becomes a handoff; no edge-length or pixel-offset matching is performed.
- Each display is identified by an opaque output ID and its owning machine's certificate fingerprint. The local OS supplies labels for the GTK editor. Windows uses the display device identity and current desktop rectangle; Hyprland uses the output name and output geometry.
- Capture events include the source display ID and crossed edge. Routing looks up the neighboring grid cell. If the neighbor belongs to the same machine, the OS handles the movement normally. If it belongs to a remote machine, the local cursor remains at the source edge and a monitor-aware enter targets the remote display's opposite edge midpoint.
- No relative offset along a shared edge is transmitted. Different monitor resolutions and heights therefore do not need pixel-level alignment.
- A bounded protocol capability/topology exchange advertises output IDs and labels after DTLS authentication. A monitor-aware enter carries the target output ID and entry edge. The receiving OS resolves that ID and places its virtual pointer at the edge midpoint. Older peers use the existing directional enter path.
- Grid edits are saved in the existing TOML configuration and propagated as versioned snapshots over authenticated peer connections. The sender retries until it receives an application-level acknowledgment and replays the newest snapshot after reconnect. A receiver acknowledges stale revisions without applying them. Concurrent offline edits are resolved by the highest Lamport revision, then by origin fingerprint as a deterministic tie-break.
- The GTK main window gets a grid editor showing local outputs in their detected arrangement and connected peer outputs as individually movable cells. An explicit **Discover displays** action establishes or reuses an authenticated connection and requests the peer's output catalog. Disconnected or missing outputs remain visible as unavailable so saved positions are not silently discarded.
- Windows and Hyprland are the supported mixed-OS pair for this change. Existing behavior remains available on other capture backends.
- Update README/DOC to explain the editor, pairing behavior, TOML representation, peer compatibility, and the fact that monitor dimensions are not matched.

## Builder decisions

- Pick a compact monitor ID representation that survives display ordering changes on Windows and output hotplug on Hyprland.
- Keep topology snapshots and grid snapshots within a strict datagram size limit; split or reject oversized topology safely.
- Keep the GTK editor consistent with existing Libadwaita patterns and expose unavailable outputs without requiring a network connection.
- Decide how a selected grid edge is restored if the OS monitor arrangement changes while the logical grid is open.

The delivery-guarantee question from the design review is settled as a design call: grid snapshots use acknowledgment, retry, and reconnect replay. Topology catalog requests can be retried by the same bounded request/response pattern.

## Open implementation risk

Hyprland currently creates layer-shell surfaces per output, then selects outputs using only the four global directions. The implementation must preserve output identity and verify that selecting a configured edge on one output does not accidentally capture the same direction on every output. Keep native same-machine output transitions under Hyprland; only capture local output edges that lead to a remote grid cell. If per-output edge selection is not reliable, report the limitation and use the available capture portal only if it exposes equivalent per-output barriers; do not silently fall back to direction-only routing while claiming monitor-grid support.
