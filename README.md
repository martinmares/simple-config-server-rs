# simple-config-server

A small Rust service that behaves like a **read‑only Spring Cloud Config Server**, but with a few extras:

* multi‑tenant environments (`dev`, `test`, `ref`, `prod`, …),
* templating of text files using environment variables (`{{ VAR_NAME }}`),
* lightweight HTML UI for inspection and debugging,
* optional **HTTP Basic Auth** and **`X-Client-Id` header based auth**,
* additional endpoints for non‑Spring clients (env JSON/export + raw assets).

It is designed to run inside Kubernetes, but works equally well as a simple binary on your laptop.

---

## 1. High‑level model

The server reads configuration from:

* one or more **Git repositories** (per environment),
* one or more **env files** (`KEY=VALUE` per line),
* optional **process environment** (real OS env vars).

For each logical environment (e.g. `dev`, `test`):

1. The server periodically fetches & resets a local Git workdir.
2. For each request, it:
   * chooses the right environment (`{env}` segment in the URL),
   * optionally chooses a Git **label** (`{label}` = branch or tag),
   * loads and templates YAML files (`application.yml`, `<application>.yml`, profile-specific variants),
   * flattens YAML into a Spring‑style JSON structure.

You can run it in two modes:

* **Single‑instance**: one Git config for everything (`git` at the root of `config.yaml`).
* **Multi‑tenant**: multiple environments under `environments:` in `config.yaml`.

---

## 2. Configuration (`config.yaml`)

### 2.1 Root structure

```yaml
http:
  bind_addr: "127.0.0.1:8899"
  base_path: "/config"   # optional prefix for all routes

# optional global env sources
env_from_process: true          # take current process env as a base map
env_file: "/app/config/global.env"

# optional auth config (Basic + X-Client-Id)
auth:
  client_id:
    enabled: true
    header_name: "x-client-id"
    clients:
      - id: "ci"
        description: "CI pipeline"
        environments: ["dev", "test"]      # or ["*"] for all envs
        scopes: ["config:read", "files:read"]
        ui_access: false
      - id: "ops"
        description: "Ops dashboards"
        environments: ["*"]
        scopes: ["config:read", "files:read", "env:read"]
        ui_access: true

# Either single-instance:
git:
  repo_url: "file:///…/config-repo"
  branch: "main"                  # default branch
  branches: ["main", "release"]   # optional list of allowed labels
  workdir: "/var/lib/simple-config-server/workdir"
  subpath: "dev"                  # optional repo subdirectory
  refresh_interval_secs: 30

# Or multi-tenant:
# environments:
#   dev:
#     git: { ... }
#     env_file: "/app/config/dev.env"
#   test:
#     git: { ... }
#     env_file: "/app/config/test.env"
```

Rules:

* If **`environments`** is present → multi‑tenant mode.
* If **`environments`** is absent and **`git`** is present → single‑instance mode.
* If `env_from_process: true`, then all OS env vars are loaded into a **global env map**.
* If root‑level `env_file` is set, it is loaded and merged into the global map.
* For each environment (`environments.<name>.env_file`), that env file is loaded and overrides global keys.

The final **template env map for a given env** is:

1. process env (if `env_from_process=true`),
2. root `env_file` (if present),
3. per‑environment `env_file` (if present).

Later values override earlier ones.

### 2.2 Git config

`GitConfig` fields:

```yaml
git:
  repo_url: "file:///…/config-repo"
  branch: "main"                  # default branch used when no label given
  branches: ["main", "release"]   # optional list of allowed labels
  workdir: "/var/lib/simple-config-server/dev"
  subpath: "dev"                  # optional path inside the repo
  refresh_interval_secs: 30       # how often to git fetch/reset (seconds)
```

Notes:

* On startup and then every `refresh_interval_secs`, the server runs a `git fetch` and hard reset to the configured ref.
* Internally, `branches` is normalized so that:
  * the default `branch` is always present,
  * the default `branch` is always the first element.

If `branches` is empty, it is treated as `["<branch>"]`.

---

## 3. Spring‑compatible endpoints

All endpoints described here are **relative to `base_path`**. For the default `base_path: "/"` you call them as written. With `base_path: "/config"` you prefix every route with `/config`.

### 3.1 URL shapes

For an environment `env`, application `app`, profile `profile`, and optional git label `label`:

* Default label (uses `git.branch`):

  ```text
  GET /{env}/{app}/{profile}
  ```

* Explicit label (branch / tag / commit-ish):

  ```text
  GET /{env}/{app}/{profile}/{label}
  ```

Example:

```bash
curl -u myuser:mypassword   "http://localhost:8899/dev/config-client/default"

curl -u myuser:mypassword   "http://localhost:8899/dev/config-client/default/release"
```

### 3.2 YAML resolution & merge order

For each request the server looks for YAML files under the environment’s `git.subpath` in this order:

1. `<app>-<profile>.yml`
2. `<app>-<profile>.yaml`
3. `<app>.yml`
4. `<app>.yaml`
5. `application-<profile>.yml`
6. `application-<profile>.yaml`
7. `application.yml`
8. `application.yaml`

Each file is:

1. loaded from Git (respecting `{label}` if given),
2. treated as **text** and templated (see section 5),
3. parsed as YAML,
4. flattened into a map of dot‑separated keys (`foo.bar.baz`) → JSON values.

For each physical YAML file found you get an entry in `propertySources`, in **the same order as above** (highest‑priority first), e.g.:

```json
{
  "name": "app",
  "profiles": ["dev"],
  "label": "release",
  "version": "86b4bdfa0feaf6d376cab620318df1f00e528314",
  "state": "",
  "propertySources": [
    {
      "name": "file:///…/config-repo/dev/config-client-dev.yml",
      "source": {
        "demo.message": "Hello from RELEASE branch (dev profile)",
        "demo.number": 42
      }
    },
    {
      "name": "file:///…/config-repo/dev/application.yml",
      "source": {
        "logging.level.org.springframework.security.ldap": "TRACE",
        "ui.config.env": "dev"
      }
    }
  ]
}
```

If **no file matches**, the server mimics Spring Cloud Config and returns HTTP `200` with an empty `propertySources` array and `label` set appropriately.

### 3.3 Data types

After templating, YAML is parsed using `serde_yaml_ng`, so basic types are preserved:

* numbers stay numbers (`1`, `1.23`),
* booleans stay booleans (`true`, `false`),
* strings stay strings.

Flattening uses an [`IndexMap`](https://docs.rs/indexmap/) under the hood, so keys in each `source` map keep their original YAML order.

---

## 4. Extra endpoints for non‑Spring clients (env + assets)

In addition to the Spring‑compatible endpoints, each environment exposes helpers for **env inspection** and **raw assets**.

Again, all routes are prefixed by `base_path` if configured.

### 4.1 Env map endpoints

For environment `{env}`:

* Effective env as JSON:

  ```text
  GET /{env}/env
  ```

  Response:

  ```json
  {
    "DB_URL": "jdbc:postgresql://localhost:5432/app-dev",
    "DB_USER": "demo_user",
    "DB_PASSWORD": "s3cr3t",
    "ENV_NAME": "local-dev"
  }
  ```

* Env as shell exports:

  ```text
  GET /{env}/env/export
  ```

  Response (plain text):

  ```bash
  export DB_URL="jdbc:postgresql://localhost:5432/app-dev"
  export DB_USER="demo_user"
  export DB_PASSWORD="s3cr3t"
  export ENV_NAME="local-dev"
  ```

### 4.2 Asset endpoints

* List all files (relative to `git.subpath`) from the default branch:

  ```text
  GET /{env}/assets
  ```

  Response:

  ```json
  {
    "files": [
      "application.yml",
      "config-client.yml",
      "user-management.yml"
    ]
  }
  ```

* Get a single asset from the **default** label:

  ```text
  GET /{env}/assets/{path}
  ```

  Example:

  ```bash
  curl -u myuser:mypassword     "http://localhost:8899/test/assets/application.yml"
  ```

* Get a single asset from an explicit label (branch / tag):

  ```text
  GET /{env}/assets/{label}/{path}
  ```

  Example:

  ```bash
  curl -u myuser:mypassword     "http://localhost:8899/test/assets/release/application.yml"
  ```

Semantics:

* The server resolves `{label}` against the `branches` list (and the default `branch`).
* Content type:
  * if the file contains a `0x00` byte or is not valid UTF‑8 → it is treated as **binary** and returned as `application/octet-stream` (or a guessed MIME type).
  * otherwise it is treated as **text**:
    * templating is applied (section 5),
    * MIME type is guessed by extension (`.yml`, `.yaml` → `text/yaml`; `.json` → `application/json`; default `text/plain`).

---

## 5. Templating

Any **text** file goes through a very small templating step:

* Pattern: `{{ VAR_NAME }}` (double curly braces, no extra syntax).
* Lookup: in the effective env map for the addressed environment.
* Missing variables are simply replaced by an empty string.

Example YAML in Git:

```yaml
demo:
  message: "Hello from {{ ENV_NAME }}!"
spring:
  datasource:
    url: "{{ DB_URL }}?currentSchema=app"
    username: "{{ DB_USER }}"
    password: "{{ DB_PASSWORD }}"
    hikari:
      maximumPoolSize: {{ DB_MAX_POOL_SIZE }}
```

With env:

```bash
DB_URL=jdbc:postgresql://localhost:5432/app-dev
DB_USER=demo_user
DB_PASSWORD=s3cr3t
DB_MAX_POOL_SIZE=30
ENV_NAME=local-dev
```

The templated YAML becomes:

```yaml
demo:
  message: "Hello from local-dev!"
spring:
  datasource:
    url: "jdbc:postgresql://localhost:5432/app-dev?currentSchema=app"
    username: "demo_user"
    password: "s3cr3t"
    hikari:
      maximumPoolSize: 30
```

After YAML parsing, `maximumPoolSize` will be a number, not a string.

> The server does **not** do any decryption.
> If you use encrypted env files (for example with `encjson-rs`), decrypt them before starting `simple-config-server` and/or render them into the `.env` files.

---

## 6. HTTP, base path & authentication

### 6.1 HTTP & base path

`http` section:

```yaml
http:
  bind_addr: "127.0.0.1:8899"
  base_path: "/config"
```

* `bind_addr` – address and port to bind, e.g. `0.0.0.0:8080`.
* `base_path` – optional prefix. If set to `/config`, all routes are available under that prefix:

  * Spring:
    * `/config/{env}/{application}/{profile}`
    * `/config/{env}/{application}/{profile}/{label}`
  * Env helpers:
    * `/config/{env}/env`
    * `/config/{env}/env/export`
  * Asset helpers:
    * `/config/{env}/assets`
    * `/config/{env}/assets/{path}`
    * `/config/{env}/assets/{label}/{path}`
  * Health:
    * `/config/healthz`
    * `/config/healthz/env`
    * `/config/healthz/env/{env}`
  * UI:
    * `/config/ui`

If `base_path` is `/`, routes are exposed exactly as `/dev/env`, `/dev/assets`, `/dev/app/default`, etc.

### 6.2 Authentication

There are two ways to protect the server:

1. **HTTP Basic Auth** via environment variables.
2. **Header‑based client auth** via `X-Client-Id` (or a custom header) configured in `config.yaml`.

You can turn on either, both, or neither.

#### 6.2.1 Basic Auth (env vars)

If you set:

```bash
export AUTH_USERNAME="myuser"
export AUTH_PASSWORD="mypassword"
```

then:

* Basic Auth is **required** for all endpoints (including `/ui`).
* A valid `Authorization: Basic ...` header **always** grants access, regardless of `X-Client-Id`.
* If credentials are wrong or missing, you get `401` with a `WWW-Authenticate` header.

If one or both env vars are missing:

* Basic Auth is **disabled**.

Credentials are not persisted anywhere; they live only in memory.

#### 6.2.2 X‑Client‑Id auth (per‑client ACL)

Header‑based auth is configured under `auth.client_id` in `config.yaml`:

```yaml
auth:
  client_id:
    enabled: true
    header_name: "x-client-id"
    clients:
      - id: "ci"
        description: "CI pipeline"
        environments: ["dev", "test"]        # or ["*"] for all envs
        scopes: ["config:read", "files:read"]
        ui_access: false
      - id: "ops-dashboard"
        environments: ["*"]
        scopes: ["config:read", "files:read", "env:read"]
        ui_access: true
```

Semantics:

* If `enabled: false` or no clients are defined:
  * X‑Client‑Id auth is effectively turned off.
* If `enabled: true`:
  * The server looks for the header named `header_name` (default `"x-client-id"`).
  * If the header value matches a configured client:
    * `environments` controls which environments the client may access:
      * `["*"]` → any environment.
      * otherwise → only listed environment names.
    * `scopes` control what the client can do:
      * `config:read` – Spring‑style endpoints (`/{env}/{app}/{profile}…`).
      * `files:read` – asset endpoints (`/{env}/assets…`).
      * `env:read` – env endpoints (`/{env}/env`, `/env/export`).
    * `ui_access: true` additionally allows access to `/ui`.
  * If the header is missing or the client is not known, the request is rejected (unless Basic Auth already succeeded or all auth is disabled).

#### 6.2.3 Auth precedence & defaults

The authorization logic is:

1. If **neither** Basic Auth nor X‑Client‑Id are configured → **open access** (backwards compatible).
2. If Basic Auth is configured and the request has **valid** Basic credentials → **allow**, ignore X‑Client‑Id.
3. Otherwise, if X‑Client‑Id auth is enabled and the header matches a configured client → check **environment** and **scopes**.
4. Otherwise → **401 Unauthorized**.

Health endpoints (`/healthz`, `/healthz/env`, `/healthz/env/{env}`) are intentionally **not** protected and always return basic status information.

---

## 7. HTML UI (`/ui`)

A dark‑themed UI (Fomantic‑UI) is available at:

* `GET /ui` (or `${base_path}/ui` if you use a prefix).

It shows:

* a list of configured environments (`dev`, `test`, `ref`, `prod`, …),
* for the selected environment:
  * Repo URL
  * Default branch
  * Subpath
  * Workdir
  * Last commit hash
  * Commit date
* effective env map:
  * as JSON,
  * as shell exports.
* a tree of **assets** (files) under the Git subpath:
  * click a file to see the **templated** content,
  * or trigger a **Spring config JSON preview** (simulates `/env/app/default` and shows the merged JSON).

All main text areas (env JSON, shell exports, templated preview, Spring JSON preview) have “copy to clipboard” icons.

Authentication:

* If Basic Auth is configured, `/ui` always requires valid Basic credentials.
* Otherwise, if X‑Client‑Id auth is enabled:
  * only clients with `ui_access: true` may access `/ui`.
* If no auth is configured at all → `/ui` is open.

---

## 8. Health endpoints

Health endpoints are useful for Kubernetes liveness/readiness probes and basic monitoring.

* Basic process health:

  ```text
  GET /healthz
  ```

  Response:

  ```json
  {
    "status": "UP",
    "startup_time": "2025-12-13T10:00:00Z"
  }
  ```

* Summary for all environments:

  ```text
  GET /healthz/env
  ```

  Response:

  ```json
  {
    "status": "UP",
    "startup_time": "2025-12-13T10:00:00Z",
    "environments": [
      {
        "env": "dev",
        "env_var_count": 42,
        "file_count": 18
      },
      {
        "env": "test",
        "env_var_count": 40,
        "file_count": 19
      }
    ]
  }
  ```

* Detail for a single environment:

  ```text
  GET /healthz/env/{env}
  ```

  Response:

  ```json
  {
    "status": "UP",
    "startup_time": "2025-12-13T10:00:00Z",
    "env": "dev",
    "env_var_count": 42,
    "file_count": 18
  }
  ```

All of the above are also available under `${base_path}` if configured (e.g. `/config/healthz`).

---

## 9. Example Git layout

A typical mono‑repo layout for multi‑tenant usage:

```text
<git_repo_root>/
  dev/
    application.yml
    config-client.yml
    user-management.yml
  test/
    application.yml
    config-client.yml
    user-management.yml
  ref/
    application.yml
    config-client.yml
    user-management.yml
  prod/
    application.yml
    config-client.yml
    user-management.yml
```

Each environment in `config.yaml` would then point `git.subpath` to the corresponding subdirectory (`"dev"`, `"test"`, …).

---

## 10. Spring Boot integration

The Spring side is standard **Config Client**:

1. Add Spring Cloud Config Client to your Spring Boot app.
2. Point it at `simple-config-server` using `spring.config.import`.

Example:

```bash
java -jar configclient.jar   --spring.profiles.active=dev   --spring.config.import="optional:configserver:http://myuser:mypass@localhost:8899/dev"
```

* The `/dev` segment selects the environment.
* Spring will first request `/dev/<app>/default`, then `/dev/<app>/dev`, etc.
* `simple-config-server` responds with the same JSON structure as the official Spring Cloud Config Server, including:
  * `name`
  * `profiles`
  * `label`
  * `version` (Git commit hash)
  * `propertySources` (list of file‑backed maps).

---

## 11. License

This project is licensed under the [MIT License](./LICENSE).
