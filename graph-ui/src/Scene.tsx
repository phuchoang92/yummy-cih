// SPDX-License-Identifier: MIT
// Instanced-mesh rendering and Barnes-Hut-informed layout visualization adapted
// from open-source graph-visualization techniques. Original patterns licensed
// under the MIT License. See the project root LICENSE for full terms.

import { Html, OrbitControls } from "@react-three/drei";
import { Canvas, useFrame } from "@react-three/fiber";
import { Bloom, EffectComposer } from "@react-three/postprocessing";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import * as THREE from "three";
import { edgeColor } from "./colors";
import type { OverviewEdge, OverviewNode } from "./types";

export interface CameraTarget { position: THREE.Vector3; lookAt: THREE.Vector3 }

export function cameraTarget(nodes: OverviewNode[], selected: Set<number>): CameraTarget | null {
  const chosen = nodes.filter((node) => selected.has(node.index));
  if (!chosen.length) return null;
  const center = chosen.reduce((sum, node) => sum.add(new THREE.Vector3(node.x, node.y, node.z)), new THREE.Vector3()).divideScalar(chosen.length);
  let spread = 0;
  for (const node of chosen) spread = Math.max(spread, center.distanceTo(new THREE.Vector3(node.x, node.y, node.z)));
  const distance = Math.max(chosen.length <= 5 ? 190 : 260, spread * 2.7);
  return { lookAt: center, position: center.clone().add(new THREE.Vector3(distance * .18, distance * .12, distance)) };
}

const ORIGIN = new THREE.Vector3();

function NodeCloud({ nodes, selected, onSelect, onHover }: {
  nodes: OverviewNode[]; selected: Set<number> | null;
  onSelect: (node: OverviewNode) => void; onHover: (node: OverviewNode | null) => void;
}) {
  const pointsRef = useRef<THREE.Points>(null);
  const maxDegree = useMemo(
    () => nodes.reduce((m, n) => Math.max(m, n.degree), 1),
    [nodes],
  );

  useLayoutEffect(() => {
    if (!pointsRef.current) return;
    const pos = new Float32Array(nodes.length * 3);
    const col = new Float32Array(nodes.length * 3);
    const c = new THREE.Color();
    nodes.forEach((node, i) => {
      pos[i * 3] = node.x; pos[i * 3 + 1] = node.y; pos[i * 3 + 2] = node.z;
      c.set(node.color);
      const t = Math.log1p(node.degree) / Math.log1p(maxDegree);
      c.multiplyScalar(1.0 + t * 3.5);
      col[i * 3] = c.r; col[i * 3 + 1] = c.g; col[i * 3 + 2] = c.b;
    });
    const geo = pointsRef.current.geometry;
    geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
    geo.setAttribute('color', new THREE.BufferAttribute(col, 3));
    geo.computeBoundingSphere();
  }, [nodes, maxDegree]);

  useLayoutEffect(() => {
    if (!pointsRef.current) return;
    const colorAttr = pointsRef.current.geometry.attributes.color as THREE.BufferAttribute | undefined;
    if (!colorAttr) return;
    const arr = colorAttr.array as Float32Array;
    const active = selected && selected.size > 0;
    const c = new THREE.Color();
    nodes.forEach((node, i) => {
      c.set(node.color);
      if (active && !selected.has(node.index)) {
        c.multiplyScalar(0.08);
      } else {
        const t = Math.log1p(node.degree) / Math.log1p(maxDegree);
        c.multiplyScalar(1.0 + t * 3.5);
      }
      arr[i * 3] = c.r; arr[i * 3 + 1] = c.g; arr[i * 3 + 2] = c.b;
    });
    colorAttr.needsUpdate = true;
  }, [selected, nodes, maxDegree]);

  return (
    <points
      ref={pointsRef}
      frustumCulled={false}
      onPointerMove={(e) => { e.stopPropagation(); if (e.index != null) onHover(nodes[e.index] ?? null); }}
      onPointerOut={() => onHover(null)}
      onClick={(e) => { e.stopPropagation(); if (e.index != null && nodes[e.index]) onSelect(nodes[e.index]); }}
    >
      <bufferGeometry />
      <pointsMaterial
        vertexColors
        size={8}
        sizeAttenuation={false}
        blending={THREE.AdditiveBlending}
        depthWrite={false}
        toneMapped={false}
      />
    </points>
  );
}

function EdgeCloud({ nodes, edges, selected }: { nodes: OverviewNode[]; edges: OverviewEdge[]; selected: Set<number> | null }) {
  const geometry = useMemo(() => {
    const byIndex = new Map(nodes.map((node) => [node.index, node]));
    const active = selected && selected.size > 0;
    const positions: number[] = [];
    const colors: number[] = [];
    const color = new THREE.Color();
    for (const edge of edges) {
      const source = byIndex.get(edge.source); const target = byIndex.get(edge.target);
      if (!source || !target) continue;
      const sourceActive = !active || selected.has(source.index);
      const targetActive = !active || selected.has(target.index);
      if (active && !sourceActive && !targetActive) continue;
      const intensity = active ? (sourceActive && targetActive ? .72 : .08) : .2;
      color.set(edgeColor(edge.kind)).multiplyScalar(intensity);
      positions.push(source.x, source.y, source.z, target.x, target.y, target.z);
      colors.push(color.r, color.g, color.b, color.r, color.g, color.b);
    }
    const result = new THREE.BufferGeometry();
    result.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
    result.setAttribute("color", new THREE.Float32BufferAttribute(colors, 3));
    return result;
  }, [nodes, edges, selected]);
  useEffect(() => () => geometry.dispose(), [geometry]);
  return <lineSegments geometry={geometry}><lineBasicMaterial vertexColors transparent blending={THREE.AdditiveBlending} depthWrite={false} toneMapped={false} /></lineSegments>;
}

function Labels({ nodes, selected }: { nodes: OverviewNode[]; selected: Set<number> | null }) {
  const visible = useMemo(() => {
    const top = [...nodes].sort((a, b) => b.degree - a.degree || a.id.localeCompare(b.id)).slice(0, 200);
    if (selected) for (const node of nodes) if (selected.has(node.index) && !top.includes(node)) top.push(node);
    return top;
  }, [nodes, selected]);
  return <>{visible.map((node) => (
    <Html key={node.id} position={[node.x, node.y + node.size, node.z]} center distanceFactor={650} style={{ pointerEvents: "none" }}>
      <span className={selected?.has(node.index) ? "star-label is-selected" : "star-label"}>{node.name || node.id}</span>
    </Html>
  ))}</>;
}

function SceneContent({ nodes, edges, selected, target, reducedMotion, autoRotate, showLabels, resetNonce, onSelect, onHover }: {
  nodes: OverviewNode[]; edges: OverviewEdge[]; selected: Set<number> | null; target: CameraTarget | null;
  reducedMotion: boolean; autoRotate: boolean; showLabels: boolean; resetNonce: number;
  onSelect: (node: OverviewNode) => void; onHover: (node: OverviewNode | null) => void;
}) {
  const controls = useRef<any>(null);
  const animTarget = useRef<CameraTarget | null>(null);

  // Begin a focus animation whenever a new camera target arrives.
  useEffect(() => { animTarget.current = target; }, [target]);
  // Reset view: cancel any focus animation and return to the initial framing.
  useEffect(() => { if (resetNonce > 0) { animTarget.current = null; controls.current?.reset(); } }, [resetNonce]);

  useFrame((state) => {
    const orbit = controls.current;
    // Auto-rotate is an explicit, opt-in toggle (reduced-motion always wins).
    if (orbit) orbit.autoRotate = autoRotate && !reducedMotion;

    // Keep point-picking tolerance ~constant in screen pixels. Points render at a
    // fixed pixel size (sizeAttenuation off), so the world-space raycast threshold
    // must scale with camera distance — otherwise a click almost never lands within
    // the default 1-unit threshold. The distance term cancels world-per-pixel.
    const cam = state.camera as THREE.PerspectiveCamera;
    const pivot = orbit ? orbit.target : ORIGIN;
    const dist = cam.position.distanceTo(pivot);
    const worldPerPixel = (2 * dist * Math.tan((cam.fov * Math.PI) / 360)) / state.size.height;
    const pointParams = state.raycaster.params.Points;
    if (pointParams) pointParams.threshold = Math.max(2, 12 * worldPerPixel);

    // Ease the camera and the orbit pivot to the focus target together, then let
    // OrbitControls own the orientation (no camera.lookAt — it would fight update()).
    const focus = animTarget.current;
    if (focus && orbit) {
      const rate = reducedMotion ? 1 : .1;
      cam.position.lerp(focus.position, rate);
      orbit.target.lerp(focus.lookAt, rate);
      orbit.update();
      if (cam.position.distanceTo(focus.position) < 1) animTarget.current = null;
    }
  });

  return <>
    <ambientLight intensity={.4} />
    <EdgeCloud nodes={nodes} edges={edges} selected={selected} />
    <NodeCloud nodes={nodes} selected={selected} onSelect={onSelect} onHover={onHover} />
    {showLabels && <Labels nodes={nodes} selected={selected} />}
    <EffectComposer><Bloom luminanceThreshold={.22} luminanceSmoothing={.72} intensity={1.25} mipmapBlur radius={.65} /></EffectComposer>
    <OrbitControls ref={controls} enableDamping dampingFactor={.08} rotateSpeed={.45} zoomSpeed={1.35} autoRotateSpeed={.6} minDistance={20} maxDistance={20_000} onStart={() => { animTarget.current = null; }} />
  </>;
}

export function hasWebGl(): boolean {
  try {
    const canvas = document.createElement("canvas");
    return !!(canvas.getContext("webgl2") || canvas.getContext("webgl"));
  } catch { return false; }
}

export function GalaxyScene(props: {
  nodes: OverviewNode[]; edges: OverviewEdge[]; selected: Set<number> | null; target: CameraTarget | null;
  autoRotate: boolean; showLabels: boolean; resetNonce: number;
  onSelect: (node: OverviewNode) => void;
}) {
  const [hovered, setHovered] = useState<OverviewNode | null>(null);
  const reducedMotion = useMemo(() => window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false, []);
  if (!hasWebGl()) return <div className="webgl-fallback"><strong>3D overview unavailable</strong><span>WebGL is disabled. Search, impact, flow, communities, and routes remain available.</span></div>;
  return <div className="galaxy-canvas">
    <Canvas camera={{ position: [0, 0, 1550], fov: 48, near: .1, far: 100_000 }} dpr={[1, 1.5]} gl={{ antialias: true, alpha: false }}>
      <color attach="background" args={["#06090f"]} />
      <SceneContent {...props} reducedMotion={reducedMotion} onHover={setHovered} />
    </Canvas>
    {hovered && <div className="node-tooltip"><span style={{ background: hovered.color }} /><strong>{hovered.name}</strong><small>{hovered.kind} · {hovered.degree} links</small></div>}
  </div>;
}
