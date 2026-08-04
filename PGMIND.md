# Knowledge Extension for PostgreSQL
## Project Handbook
Version: 0.1

---

# Project Vision

This project aims to build a PostgreSQL extension that introduces **Knowledge** as a first-class database abstraction.

We are not building another database.

We are extending PostgreSQL in the same way that:

- PostGIS introduced Geometry.
- JSONB introduced Documents.
- pgvector introduced Vectors.

Our extension introduces Knowledge.

---

# Mission

Developers should not need to assemble:

- PostgreSQL
- Filesystem
- Vector DB
- Graph DB
- Search Engine
- Chunking Pipeline
- Prompt Builder
- RAG Framework

to build AI applications.

Knowledge should live inside PostgreSQL.

---

# Long-Term Goal

A developer should eventually be able to write:

```sql
SELECT knowledge.context(
    query => 'How authentication works',
    token_budget => 12000
);
```

instead of building an entire retrieval pipeline.

---

# Philosophy

## Knowledge is the source of truth

Not files.

Not vectors.

Not embeddings.

Not prompts.

Knowledge.

---

## Markdown is an interface

Markdown is not storage.

Markdown is a serialization format.

Internally the database stores structured knowledge.

---

## AI is a consumer

LLMs consume knowledge.

They do not define truth.

Every AI-generated artifact must preserve provenance.

---

## Context is the product

SQL returns rows.

Knowledge databases return context.

---

## Semantic APIs

Applications describe intent.

They never choose retrieval algorithms.

The planner does.

---

## Immutable knowledge

Knowledge is append-only.

History is permanent.

Updates create revisions.

---

## Stable identities

Every heading.

Every paragraph.

Every table.

Every list.

Every code block.

Receives a permanent identifier.

---

# Architectural Principles

Everything builds in layers.

```
Applications

↓

Knowledge API

↓

Planner

↓

Indexes

↓

Knowledge Objects

↓

Markdown AST

↓

PostgreSQL
```

No layer bypasses another.

---

# Project Structure

```
docs/

    PROJECT.md

    roadmap.md

    architecture/

    rfc/

    phases/

extension/

parser/

planner/

storage/

tests/
```

---

# Documentation Strategy

Every major architectural decision must be documented.

No implementation starts without documentation.

---

# RFC Strategy

Every significant feature receives its own RFC.

RFCs are immutable after acceptance.

Changes require a new RFC.

RFC numbering follows:

```
RFC-000 Vision

RFC-001 Native Markdown

RFC-002 Knowledge Objects

RFC-003 Storage Engine

RFC-004 Versioning

RFC-005 Query Planner

RFC-006 Context Planner

RFC-007 Hybrid Search

RFC-008 Semantic Indexes

RFC-009 Graph Model

RFC-010 Semantic Editing

...
```

---

# Documentation Hierarchy

```
PROJECT.md

↓

Architecture Documents

↓

RFCs

↓

Phase Designs

↓

Implementation Tasks

↓

Code
```

Code never becomes the documentation.

Documentation drives the code.

---

# Documents To Write

## Core Documents

PROJECT.md

roadmap.md

architecture.md

design-principles.md

storage-overview.md

query-engine.md

planner-overview.md

knowledge-model.md

markdown-model.md

versioning.md

security.md

performance.md

parser.md

extension-api.md

testing-strategy.md

---

## RFCs

RFC-000 Vision

RFC-001 Native Markdown Type

RFC-002 Knowledge Object Model

RFC-003 Storage Layer

RFC-004 Block Model

RFC-005 Version Engine

RFC-006 Context Planner

RFC-007 Search Planner

RFC-008 Semantic Indexing

RFC-009 Query API

RFC-010 AI Editing

RFC-011 Provenance

RFC-012 Permission Model

RFC-013 Extension Packaging

RFC-014 Performance

RFC-015 Distributed Storage

---

# Development Phases

## Phase 1

Native Markdown Type

Deliverables

- markdown type
- parser
- AST
- renderer
- validator
- stable block ids
- diff engine

No AI.

No vectors.

No graph.

---

## Phase 2

Knowledge Objects

Every block becomes addressable.

Knowledge object storage.

Relationships.

---

## Phase 3

Version Engine

Immutable revisions.

Delta storage.

History.

---

## Phase 4

Knowledge Graph

Automatic relationship generation.

References.

Dependencies.

---

## Phase 5

Semantic Indexes

Embeddings.

Entities.

Keywords.

Summaries.

Citations.

---

## Phase 6

Hybrid Search

Planner combines

- vectors
- graph
- bm25
- metadata

---

## Phase 7

Context Planner

Prompt compilation.

Token budgeting.

Deduplication.

Compression.

Ordering.

---

## Phase 8

Semantic Editing

Instruction-driven updates.

Automatic graph maintenance.

Automatic summary updates.

Automatic embedding updates.

---

## Phase 9

Distributed Knowledge

Replication.

Synchronization.

Federated search.

---

# Product Rules

The extension should feel like PostgreSQL.

Avoid introducing new concepts unless necessary.

Prefer SQL.

Prefer PostgreSQL idioms.

Avoid hidden behavior.

Deterministic APIs first.

AI-enhanced APIs second.

---

# Coding Rules

Parser first.

Storage second.

Indexes third.

Planner fourth.

AI last.

Never build higher layers before lower layers are stable.

---

# AI Agent Roles

## Product Manager

Owns

- roadmap
- RFC acceptance
- priorities
- scope

Never writes implementation.

---

## Architect

Owns

- system architecture
- RFC reviews
- long-term consistency

---

## Storage Engineer

Owns

- PostgreSQL extension
- storage
- WAL
- indexing
- transactions

---

## Parser Engineer

Owns

Markdown parsing.

AST.

Serialization.

Rendering.

---

## Query Engineer

Owns

SQL APIs.

Operators.

Functions.

Execution planning.

---

## Semantic Engineer

Owns

Embeddings.

Entity extraction.

Summaries.

Knowledge graph.

---

## Planner Engineer

Owns

Context generation.

Search planning.

Prompt optimization.

---

## Documentation Agent

Owns

RFCs.

Examples.

API docs.

Tutorials.

---

# Definition of Done

A phase is complete when:

- RFC accepted
- Documentation complete
- Tests written
- Benchmarks passed
- API documented
- Examples included

---

# Success Metric

The project succeeds when developers stop saying:

"I need PostgreSQL + Pinecone + Neo4j + Elasticsearch + LangChain."

and instead say:

"I installed the Knowledge extension."

---

# Motto

> PostgreSQL stores data.

> Knowledge stores understanding.

Our mission is to teach PostgreSQL how to understand.