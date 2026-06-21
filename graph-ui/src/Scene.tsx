// SPDX-License-Identifier: MIT
// Instanced-mesh rendering and Barnes-Hut-informed layout visualization adapted
// from open-source graph-visualization techniques. Original patterns licensed
// under the MIT License. See the project root LICENSE for full terms.

import { Html, OrbitControls } from "@react-three/drei";
import { Canvas, useFrame, useThree } from "@react-three/fiber";
import { Bloom, EffectComposer } from "@react-three/postprocessing";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import * as THREE from "three";
import type { OverviewEdge, OverviewNode } from "./types";

const EDGE_COLORS: Record<string, string> = {
  CALLS: "#1da27e", HANDLES_ROUTE: "#eab308", IMPORTS: "#3b82f6",
  EXTENDS: "#f97316", IMPLEMENTS: "#a855f7", EXTERNAL_CALL: "#e11d48",
  PUBLISHES_EVENT: "#ec4899", LISTENS_TO: "#ec4899", INTEGRATION_LINK: "#06b6d4",
  READS_TABLE: "#60a5fa", WRITES_TABLE: "#fb7185", TESTS: "#22d3ee",
};

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

function CameraAnimator({ target, reducedMotion }: { target: CameraTarget | null; reducedMotion: boolean }) {
  const { camera } = useThree();
  const current = useRef<CameraTarget | null>(null);
  useEffect(() => { current.current = target; }, [target]);
  useFrame(() => {
    if (!current.current) return;
    const rate = reducedMotion ? 1 : .085;
    camera.position.lerp(current.current.position, rate);
    camera.lookAt(current.current.lookAt);
    if (camera.position.distanceTo(current.current.position) < 1) current.current = null;
  });
  return null;
}

function NodeCloud({ nodes, selected, onSelect, onHover }: {
  nodes: OverviewNode[]; selected: Set<number> | null;
  onSelect: (node: OverviewNode) => void; onHover: (node: OverviewNode | null) => void;
}) {
  const mesh = useRef<THREE.InstancedMesh>(null);
  const object = useMemo(() => new THREE.Object3D(), []);
  const color = useMemo(() => new THREE.Color(), []);

  useLayoutEffect(() => {
    if (!mesh.current) return;
    nodes.forEach((node, position) => {
      object.position.set(node.x, node.y, node.z);
      const scale = Math.max(1.5, node.size * .42);
      object.scale.setScalar(scale);
      object.updateMatrix();
      mesh.current!.setMatrixAt(position, object.matrix);
    });
    mesh.current.instanceMatrix.needsUpdate = true;
    mesh.current.computeBoundingSphere();
  }, [nodes, object]);

  useLayoutEffect(() => {
    if (!mesh.current) return;
    const active = selected && selected.size > 0;
    nodes.forEach((node, position) => {
      color.set(node.color);
      if (active && !selected.has(node.index)) color.multiplyScalar(.1);
      else color.multiplyScalar(1.45);
      mesh.current!.setColorAt(position, color);
    });
    if (mesh.current.instanceColor) mesh.current.instanceColor.needsUpdate = true;
  }, [nodes, selected, color]);

  return (
    <instancedMesh
      ref={mesh} args={[undefined, undefined, nodes.length]} frustumCulled={false}
      onPointerMove={(event) => { event.stopPropagation(); if (event.instanceId !== undefined) onHover(nodes[event.instanceId] ?? null); }}
      onPointerOut={() => onHover(null)}
      onClick={(event) => { event.stopPropagation(); if (event.instanceId !== undefined && nodes[event.instanceId]) onSelect(nodes[event.instanceId]); }}
    >
      <sphereGeometry args={[1, 12, 8]} />
      <meshBasicMaterial vertexColors toneMapped={false} />
    </instancedMesh>
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
      color.set(EDGE_COLORS[edge.kind] ?? "#1c8585").multiplyScalar(intensity);
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

function SceneContent({ nodes, edges, selected, target, reducedMotion, onSelect, onHover }: {
  nodes: OverviewNode[]; edges: OverviewEdge[]; selected: Set<number> | null; target: CameraTarget | null;
  reducedMotion: boolean; onSelect: (node: OverviewNode) => void; onHover: (node: OverviewNode | null) => void;
}) {
  const controls = useRef<any>(null);
  const lastInteraction = useRef(Date.now());
  useFrame(() => { if (controls.current) controls.current.autoRotate = !reducedMotion && Date.now() - lastInteraction.current > 60_000; });
  return <>
    <ambientLight intensity={.4} />
    <EdgeCloud nodes={nodes} edges={edges} selected={selected} />
    <NodeCloud nodes={nodes} selected={selected} onSelect={onSelect} onHover={onHover} />
    <Labels nodes={nodes} selected={selected} />
    <CameraAnimator target={target} reducedMotion={reducedMotion} />
    <EffectComposer><Bloom luminanceThreshold={.22} luminanceSmoothing={.72} intensity={1.25} mipmapBlur radius={.65} /></EffectComposer>
    <OrbitControls ref={controls} enableDamping dampingFactor={.08} rotateSpeed={.45} zoomSpeed={1.35} minDistance={20} maxDistance={20_000} onStart={() => { lastInteraction.current = Date.now(); }} />
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
