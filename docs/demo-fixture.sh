#!/usr/bin/env bash
# Builds the throwaway repo docs/demo.tape records against: a small
# TypeScript project whose working tree holds one tangled, uncommitted
# "AI agent" change — an options-object refactor, a formatting feature
# built on it, test updates, lockfile churn (the [noise] unit), and one
# deliberate mistake (a stale positional call) so the diagnostics money
# shot has something real to show.
#
# Usage: docs/demo-fixture.sh <target-dir>
#
# After running this, seed the review-units cache once so the recording's
# `u` opens instantly from cache instead of sitting through an agent run:
#
#   cd <target-dir> && ktmr diff --dump-units
#
# (needs `claude` or `codex` on PATH; re-run with --regroup if the
# grouping comes back with labels you don't like).

set -euo pipefail

dir=${1:?usage: demo-fixture.sh <target-dir>}
mkdir -p "$dir"
cd "$dir"

git init -q
git config user.name "demo"
git config user.email "demo@example.com"

mkdir -p src test

cat > package.json <<'EOF'
{
  "name": "acme-users",
  "private": true,
  "version": "1.0.0"
}
EOF

cat > package-lock.json <<'EOF'
{
  "name": "acme-users",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "acme-users", "version": "1.0.0" },
    "node_modules/left-pad": { "version": "1.3.0", "resolved": "https://registry.example/left-pad-1.3.0.tgz" },
    "node_modules/is-even": { "version": "1.0.0", "resolved": "https://registry.example/is-even-1.0.0.tgz" }
  }
}
EOF

cat > tsconfig.json <<'EOF'
{
  "compilerOptions": {
    "strict": true,
    "target": "es2022",
    "module": "es2022",
    "moduleResolution": "bundler"
  },
  "include": ["src", "test"]
}
EOF

cat > src/user.ts <<'EOF'
export interface User {
  name: string;
  email: string;
  age: number;
}

export function createUser(name: string, email: string, age: number): User {
  return { name, email, age };
}
EOF

cat > src/api.ts <<'EOF'
import { createUser } from "./user";

export function registerUser(name: string, email: string, age: number) {
  const user = createUser(name, email, age);
  return user;
}

export function registerAdmin(name: string, email: string) {
  const admin = createUser(name, email, 0);
  return admin;
}
EOF

cat > src/format.ts <<'EOF'
import { User } from "./user";

export function formatUser(user: User): string {
  return `${user.name} <${user.email}>`;
}
EOF

cat > test/user.test.ts <<'EOF'
import { createUser } from "../src/user";

export function testCreateUser() {
  const u = createUser("Ada", "ada@example.com", 36);
  if (u.name !== "Ada") throw new Error("name");
}
EOF

git add -A
git commit -qm "acme-users: user registration service"

# ---- the "AI agent's" uncommitted change: four concerns in one diff ----

# 1. The refactor: createUser takes an options object now.
cat > src/user.ts <<'EOF'
export interface User {
  name: string;
  email: string;
  age: number;
  admin: boolean;
}

export interface CreateUserOptions {
  name: string;
  email: string;
  age: number;
  admin?: boolean;
}

export function createUser(options: CreateUserOptions): User {
  const { name, email, age } = options;
  return { name, email, age, admin: options.admin ?? false };
}
EOF

# 2. One call site migrated correctly; the other touched but left on the
#    old positional shape — the deliberate mistake `]d` lands on.
cat > src/api.ts <<'EOF'
import { createUser } from "./user";

export function registerUser(name: string, email: string, age: number) {
  const user = createUser({ name, email, age });
  return user;
}

export function registerAdmin(name: string, email: string) {
  const admin = createUser(name, email, 0, true);
  return admin;
}
EOF

# 3. The feature built on the refactor (with one deliberately long line
#    for the soft-wrap continuation marker).
cat > src/format.ts <<'EOF'
import { User } from "./user";

export function formatUser(user: User): string {
  return `${user.name} <${user.email}>`;
}

export function formatUserSummary(user: User): string {
  return `${user.name} <${user.email}> — age ${user.age}, ${user.admin ? "administrator with full access to every project and setting" : "regular member with read-and-comment access to shared projects"}`;
}
EOF

# 4. Tests catching up with the refactor.
cat > test/user.test.ts <<'EOF'
import { createUser } from "../src/user";

export function testCreateUser() {
  const u = createUser({ name: "Ada", email: "ada@example.com", age: 36 });
  if (u.name !== "Ada") throw new Error("name");
  if (u.admin) throw new Error("admin should default to false");
}

export function testAdminFlag() {
  const u = createUser({ name: "Grace", email: "grace@example.com", age: 45, admin: true });
  if (!u.admin) throw new Error("admin");
}
EOF

# 5. Lockfile churn — what the [noise] unit exists to sweep aside.
cat > package-lock.json <<'EOF'
{
  "name": "acme-users",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "acme-users", "version": "1.0.0" },
    "node_modules/left-pad": { "version": "1.3.1", "resolved": "https://registry.example/left-pad-1.3.1.tgz" },
    "node_modules/is-even": { "version": "1.0.1", "resolved": "https://registry.example/is-even-1.0.1.tgz" },
    "node_modules/is-odd": { "version": "3.0.1", "resolved": "https://registry.example/is-odd-3.0.1.tgz" }
  }
}
EOF

echo "demo fixture ready: $dir"
