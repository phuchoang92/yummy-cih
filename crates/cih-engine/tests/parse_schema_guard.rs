//! Golden-corpus guard for `cih_lang::PARSE_CACHE_SCHEMA`.
//!
//! The parse cache serves `ParsedUnit`s keyed on (file bytes × schema). If a
//! parser/extractor changes its output without a schema bump, every unchanged
//! file silently keeps its stale cached unit after an upgrade — the exact bug
//! this guard exists to prevent. The fixture corpus below exercises one
//! extraction family per language; its serialized output hash is pinned
//! TOGETHER with the schema number.
//!
//! When this test fails you MUST do both:
//!   1. bump `cih_lang::PARSE_CACHE_SCHEMA` (crates/cih-lang/src/lib.rs), and
//!   2. update `GOLDEN` below to `(new_schema, new_hash)` — the failure
//!      message prints the new hash.

use std::fs;
use std::path::PathBuf;

/// (expected PARSE_CACHE_SCHEMA, blake3-16 of the corpus parse output).
const GOLDEN: (u32, &str) = (17, "70d8c239044b2329");

const FIXTURES: &[(&str, &str)] = &[
    (
        "src/main/java/com/acme/OrderController.java",
        r#"package com.acme;
import org.springframework.web.bind.annotation.*;
import org.springframework.web.client.RestTemplate;

@RestController
@RequestMapping("/api/orders")
public class OrderController {
    private final RestTemplate rest = new RestTemplate();

    @GetMapping("/{id}")
    public String get(@PathVariable String id) {
        return rest.getForObject("https://inventory/api/stock/" + id, String.class);
    }
}
"#,
    ),
    (
        "src/main/kotlin/com/acme/PingController.kt",
        r#"package com.acme

import org.springframework.web.bind.annotation.*

@RestController
@RequestMapping("/api/ping")
class PingController {
    @GetMapping("/{id}")
    fun ping(@PathVariable id: String): String = "pong"
}
"#,
    ),
    (
        "src/services/apiClient.ts",
        r#"export const API_BASE_URL = import.meta.env.VITE_API_URL ?? '/api/v1';

export const apiFetch = async (endpoint: string, options = {}, token?: string) => {
    const url = `${API_BASE_URL}${endpoint}`;
    try {
        return await fetch(url, { ...options });
    } catch (e) {
        throw e;
    }
};
"#,
    ),
    (
        "src/services/caller.ts",
        r#"import { apiFetch } from './apiClient';
import * as apins from './apiClient';
export const createItem = (body: any, token: string) =>
    apiFetch('/items', { method: 'POST' }, token);
export const viaNamespace = (token: string) =>
    apins.apiFetch('/ns-items', { method: 'PATCH' }, token);
"#,
    ),
    (
        "src/services/client.ts",
        r#"const API_BASE_URL = 'http://localhost:8080/api';
export async function load(id: string) {
  const r = await fetch(`${API_BASE_URL}/items/${id}`);
  return r.json();
}
"#,
    ),
    (
        // camelCase in-file const in a `${…}` template folds (ConstRef); the `${id}`
        // param stays Dynamic.
        "src/services/lower.ts",
        r#"const apiBase = '/api/v2';
export async function get(id: string) {
  return fetch(`${apiBase}/items/${id}`);
}
"#,
    ),
    (
        "services/api_client.py",
        r#"import os
import requests

API_BASE = os.environ.get("API_URL", "/api/v1")

def api_get(path):
    url = f"{API_BASE}{path}"
    return requests.get(url)

def api_post(path, data):
    return requests.post(API_BASE + path, json=data)
"#,
    ),
    (
        "services/caller.py",
        r#"from services.api_client import api_get
import services.api_client as api

def load(item_id):
    return api_get(f"/admin/items/{item_id}")

def load_via_alias(item_id):
    return api.api_get(f"/alias/items/{item_id}")
"#,
    ),
    (
        "src/app/client.py",
        r#"import requests

API_BASE = "/api/v1"

def load(item_id):
    return requests.get(f"{API_BASE}/items/{item_id}").json()
"#,
    ),
    (
        // JavaScript (parsed by the TypeScript provider): Express route + an
        // outbound fetch call + CommonJS export.
        "src/server.js",
        r#"const express = require('express');
const app = express();

async function getStock(id) {
    const r = await fetch(`http://inventory/api/stock/${id}`);
    return r.json();
}

app.get('/api/orders/:id', async (req, res) => {
    res.json(await getStock(req.params.id));
});

module.exports = app;
"#,
    ),
    (
        // Fastify (import-gated verb call + config-object route).
        "src/fastify-app.ts",
        r#"import fastify from 'fastify';
const app = fastify();
app.get('/api/users/:id', async (req) => ({ id: req.params.id }));
app.route({ method: ['GET', 'POST'], url: '/api/items' });
"#,
    ),
    (
        // Next.js App Router (file-based: exported verb handlers).
        "src/app/api/orders/[id]/route.ts",
        r#"export async function GET() { return Response.json({ ok: true }); }
export async function POST() { return Response.json({ ok: true }); }
"#,
    ),
    (
        // GraphQL resolver → Route nodes (QUERY/MUTATION) + HandlesRoute edges.
        "src/user.resolver.ts",
        r#"import { Resolver, Query, Mutation } from 'type-graphql';
@Resolver()
export class UserResolver {
    @Query()
    users() { return []; }
    @Mutation()
    createUser() { return {}; }
}
"#,
    ),
    (
        // React arrow-const component + hook (P4 arrow-const gap) → Function nodes
        // with stereotypes; the fetch attributes to the component, not the file.
        "src/ui.tsx",
        r#"import React from 'react';
export const UserCard = ({ id }) => {
    fetch(`/api/users/${id}`);
    return null;
};
export const useUser = (id) => id;
"#,
    ),
    (
        // Messaging: kafkajs publish/subscribe → EventPublish/EventListen contracts.
        "src/kafka.ts",
        r#"import { Kafka } from 'kafkajs';
export async function pub(producer) { await producer.send({ topic: 'orders', messages: [] }); }
export async function sub(consumer) { await consumer.subscribe({ topic: 'orders' }); }
"#,
    ),
    (
        // Component stereotype + constructor DI (NestJS provider → TypeRef refs).
        "src/user.service.ts",
        r#"import { Injectable } from '@nestjs/common';
@Injectable()
export class UserService {
    constructor(private readonly repo: UserRepository) {}
}
@Injectable()
export class UserRepository {}
"#,
    ),
    (
        // ORM DB access: Prisma query (table + read/write edges) + TypeORM entity.
        "src/db.ts",
        r#"import { PrismaClient } from '@prisma/client';
import { Entity } from 'typeorm';
const prisma = new PrismaClient();
export async function list() { return prisma.user.findMany(); }
export async function add(d) { return prisma.order.create({ data: d }); }
@Entity('products')
class Product {}
"#,
    ),
    (
        // Outbound clients: axios.create() instance (baseURL fold) + Angular HttpClient.
        "src/http-clients.ts",
        r#"import axios from 'axios';
import { HttpClient } from '@angular/common/http';
const api = axios.create({ baseURL: '/api/v1' });
export async function load() { return api.get('/orders/1'); }
class Svc {
    constructor(private http: HttpClient) {}
    users() { return this.http.get('/api/users'); }
}
"#,
    ),
    (
        "cmd/server/main.go",
        r#"package main

import "net/http"

func main() {
    http.HandleFunc("GET /orders/{id}", handleOrder)
    http.ListenAndServe(":8080", nil)
}

func handleOrder(w http.ResponseWriter, r *http.Request) {
    http.Get("http://inventory/api/stock")
}
"#,
    ),
];

#[test]
fn parser_output_changes_require_schema_bump() {
    let dir = std::env::temp_dir().join(format!(
        "cih-schema-guard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut rels: Vec<String> = Vec::new();
    for (rel, source) in FIXTURES {
        let path: PathBuf = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, source).unwrap();
        rels.push((*rel).to_string());
    }

    let mut registry = cih_parse::LanguageRegistry::new();
    for provider in cih_lang::all_providers() {
        registry.register_boxed(provider);
    }
    let output = cih_parse::parse_file_units(&dir, &rels, &registry).unwrap();
    fs::remove_dir_all(&dir).ok();
    assert!(
        output.skipped.is_empty(),
        "guard corpus must parse cleanly: {:?}",
        output.skipped
    );

    // serde_json's default map is sorted and unit order follows `rels` order,
    // so the serialization is deterministic.
    let serialized = serde_json::to_string(&output.units).unwrap();
    let hash = blake3::hash(serialized.as_bytes()).to_hex()[..16].to_string();

    assert_eq!(
        GOLDEN.0,
        cih_lang::PARSE_CACHE_SCHEMA,
        "GOLDEN schema is out of sync with cih_lang::PARSE_CACHE_SCHEMA — \
         update GOLDEN to (PARSE_CACHE_SCHEMA, corpus hash) as a pair"
    );
    assert_eq!(
        GOLDEN.1,
        hash,
        "parser output changed for the guard corpus — bump \
         cih_lang::PARSE_CACHE_SCHEMA (crates/cih-lang/src/lib.rs) and update \
         GOLDEN to ({}, \"{hash}\")",
        cih_lang::PARSE_CACHE_SCHEMA + 1
    );
}
