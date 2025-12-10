# Secure Config Server

A small Rust service that acts as a **secure, Spring Cloud Config-compatible server** with a few extra features:

* Serves configuration from one or more Git repositories (local `file://` or remote).
* Mostly drop-in compatible with **Spring Cloud Config Server JSON endpoints**, with an extra **environment segment**:
  * `/{env}/{application}/{profile}`
  * `/{env}/{application}/{profile}/{label}`
* Supports **labels as Git refs** (branches/tags/commits).
* Supports **search paths** via configurable `subpath` (like `search-paths` in Spring).
* Extra **file and env endpoints** for non-Spring clients (shell, Python, C++, …).
* Simple templating for text files using `{{ VAR }}` placeholders filled from environment variables.
* Optional **HTTP Basic Auth** using environment variables.
* Small HTML UI on `/ui` with per-environment repo / branch / commit information, env preview and templated file preview.
* Structured logging via `tracing`.

---

## 1. High-level architecture

The server is stateless and reads all configuration from Git repositories. It supports two configuration modes:

* **Single-instance** – one Git repo + one logical environment (`default`).
* **Multi-tenant** – multiple logical environments (`dev`, `test`, `ref`, `prod`, …), each with its own Git config and optional env file.

On startup it:

* Parses `config.yaml`.
* Determines whether it is running in **single-instance** or **multi-tenant** mode.
* Clones or fetches the configured Git repository/ies into local `workdir` directories.
* Builds a **global environment map** (from `env_from_process` / global `env_file`).
* For each configured environment, builds an **effective env map**:
  * `global env` ∪ `environment-specific env_file` (later values override earlier ones).

In the background it periodically runs `git fetch` / `git reset --hard origin/<branch>` for each environment to keep the local `workdir` in sync.

For each HTTP request it:

* Resolves the Git **ref** (branch/tag/commit) based on the `label` parameter (or default branch if `label` is missing).
* Reads the requested file(s) via `git show <ref>:<subpath>/<path>` – no local checkout switching.
* For text files, applies `{{ VAR }}` templating using the environment map for the addressed environment.
* For Spring-style requests, flattens YAML into a JSON structure compatible with Spring Cloud Config.

Git is accessed via the `git` CLI, so **`git` must be installed** and on `PATH`.

---

## 2. Configuration (`config.yaml`)

The root configuration looks like this:

```yaml
http:
  bind_addr: "127.0.0.1:8899"
  base_path: "/"             # e.g. "/config" behind a reverse proxy

# optional global env sources
env_from_process: true       # take all process env vars
env_file: "/app/config/common.env"   # additional KEY=VALUE pairs

# EITHER single-instance...
git:
  repo_url: "file:///…/example-configs"
  branch: "main"
  workdir: "/…/config.wrk"
  subpath: "test"
  refresh_interval_secs: 30

# ...OR multi-tenant (if `environments` is present, it wins)
environments:
  dev:
    git:
      repo_url: "file:///…/example-configs"
      branch: "main"
      workdir: "/…/config-dev.wrk"
      subpath: "dev"
      refresh_interval_secs: 30
    env_file: "/app/config/dev.env"

  test:
    git:
      repo_url: "file:///…/example-configs"
      branch: "main"
      workdir: "/…/config-test.wrk"
      subpath: "test"
      refresh_interval_secs: 30
    env_file: "/app/config/test.env"
```

### 2.1 Single-instance vs multi-tenant

The mode is determined automatically:

* **Single-instance**:
  * `git` section present.
  * `environments` map is **empty**.
  * The server exposes a single logical environment named **`default`**.
  * Spring endpoints become:
    * `/{env}/{application}/{profile}` → `/default/{application}/{profile}`
    * `/{env}/{application}/{profile}/{label}` → `/default/{application}/{profile}/{label}`

* **Multi-tenant**:
  * `environments` map is **non-empty**.
  * Each key under `environments` is an environment name (`dev`, `test`, `ref`, `prod`, …).
  * Each environment has its own `git` and optional `env_file`.
  * Spring endpoints are called with the environment explicitly:
    * `/dev/config-client/dev`
    * `/test/user-management/default`
    * `/prod/config-client/dev/release`
  * Single `git` section at root is ignored when `environments` is present.

### 2.2 Global vs per-environment env

Environment variables used for templating are built as:

```text
effective_env(env_name) = global_env ∪ env_specific_env_file(env_name)
```

Where:

* `global_env` is composed of:
  * all process variables if `env_from_process: true`
  * plus all KEY=VALUE pairs from global `env_file` (if specified)
* `env_specific_env_file(env_name)`:
  * KEY=VALUE pairs from `environments.<env_name>.env_file` (if specified)

Per-environment values **override** global ones in case of duplicate keys.

---

## 3. Spring Cloud Config compatibility

The main goal is to behave like Spring Cloud Config for the JSON environment endpoints, but with an explicit environment prefix.

### 3.1 Endpoints

Supported JSON endpoints:

* `GET /{env}/{application}/{profile}`
* `GET /{env}/{application}/{profile}/{label}`

(plus an optional `base_path` prefix, see below.)

The response has the same shape as Spring Cloud Config’s `/env` endpoint:

```json
{
  "name": "config-client",
  "profiles": ["dev"],
  "label": "main",
  "version": "b4e0c15a536cc16b6b0c2a78c188bfaa3246704c",
  "state": "",
  "propertySources": [
    {
      "name": "git:file:///path/to/repo/config-client-dev.yml",
      "source": {
        "demo.message": "Hello from MAIN branch (dev profile)",
        "demo.number": 42
      }
    }
  ]
}
```

Notes:

* `env` is an arbitrary string chosen in `config.yaml` (e.g. `dev`, `test`, `ref`, `prod`, or `default` in single-instance mode).
* `label` is:
  * `null` when no label is provided.
  * The string from the URL when provided (e.g. `release`).
* `version` is the **commit hash** (`git rev-parse`) of the resolved ref.
* `propertySources`:
  * Contains a single flattened source if any config files were found.
  * Is an **empty array** when no files are found – HTTP **200** with empty `propertySources` (like Spring), not 404.

### 3.2 Label → Git ref

The label is resolved as:

* If label is present:
  * try `<label>`
  * then `origin/<label>`
* If label is absent:
  * use the configured default branch `git.branch`
  * or `origin/<git.branch>`

All file reads are done using:

```bash
git -C <workdir> show <ref>:<subpath>/<path>
```

so multiple environments and labels/branches can be requested concurrently.

### 3.3 Search paths (`subpath`)

Spring has `search-paths`. Here we use:

```yaml
git:
  subpath: "test"
```

This acts as the **root within the repo** for that environment. All file lookups behave as if the repo root was `<repo>/<subpath>`.

Example:

* repo: `file:///…/example-configs`
* `subpath: "test"`
* config file in Git: `test/config-client-dev.yml`
* request: `GET /test/config-client/dev`
* server will read `config-client-dev.yml` from `ref:test/config-client-dev.yml`.

---

## 4. Extra endpoints for non-Spring clients

In addition to Spring-style endpoints, each environment exposes:

* **File endpoint** (with templating):
  * `GET /{env}/file/{label}/{*path}`

* **Environment map**:
  * `GET /{env}/env` – JSON map of all variables used for templating.
  * `GET /{env}/env/export` – shell `export VAR="value"` lines.

* **File listing**:
  * `GET /{env}/files` – JSON list of files under the environment’s `subpath`.

Again, all of the above are prefixed by `base_path` if configured.

### 4.1 File endpoint semantics

* Uses the same label → Git ref resolution as the Spring endpoints.
* Combines `subpath` and `{*path}` and reads via `git show`.

Content types:

* If the file looks like **binary** (null byte or non-UTF-8) → returns `application/octet-stream` (or guessed MIME) as raw bytes.
* If the file is **text**:
  * Applies templating: `{{ VAR_NAME }}` → replaced with value from the effective env map of the addressed environment.
  * Returns text with a guessed MIME type (based on extension) or `text/plain`.

Typical use cases:

```bash
# get env for an environment as JSON
curl -s -u "$AUTH_USERNAME:$AUTH_PASSWORD"   "http://localhost:8899/dev/env" | jq

# load env into shell
eval "$(
  curl -s -u "$AUTH_USERNAME:$AUTH_PASSWORD"     "http://localhost:8899/test/env/export"
)"

# preview a templated YAML file for env 'ref'
curl -s -u "$AUTH_USERNAME:$AUTH_PASSWORD"   "http://localhost:8899/ref/file/main/user-management.yml"
```

---

## 5. Templating

Any **text** file is processed before sending:

* Pattern: `{{ VAR_NAME }}` (double curly braces).
* The server uses a pre-built `HashMap<String, String>` **per environment**.
* Values are looked up by exact variable name.

Example in Git:

```yaml
spring:
  datasource:
    url: "{{ TSM_DB_URL }}?currentSchema=um"
    username: "{{ TSM_DB_USER }}"
    password: "{{ TSM_DB_PASSWORD }}"
```

If `config.yaml` contains:

```yaml
env_from_process: true
env_file: "/app/config/common.env"

environments:
  test:
    git: ...
    env_file: "/app/config/test.env"
```

and you prepare:

```bash
# global env (used for all environments)
export TSM_DB_USER=demo_user
export TSM_DB_PASSWORD=s3cr3t

# per-env .env file
cat > /app/config/test.env <<'EOF'
TSM_DB_URL=jdbc:postgresql://localhost:5432/app-test
TSM_ENV_NAME=lokalni-test
EOF
```

Then requests against `/test/...` will see:

* `TSM_DB_USER`, `TSM_DB_PASSWORD` from process/global env.
* `TSM_DB_URL`, `TSM_ENV_NAME` from `/app/config/test.env`.

> The server **does not decrypt secrets itself**.
> If you have encrypted files (e.g. `env.secured.json`), decrypt them before starting the server and/or render them into env files.

---

## 6. HTTP / base path / authentication

### 6.1 HTTP

`http` section:

```yaml
http:
  bind_addr: "127.0.0.1:8899"
  base_path: "/config"
```

* `bind_addr` – address and port to bind, e.g. `0.0.0.0:8080`.
* `base_path` – optional prefix path. If set to `/config`, all routes are available under that prefix:

  * `/config/{env}/{application}/{profile}`
  * `/config/{env}/{application}/{profile}/{label}`
  * `/config/{env}/file/{label}/{*path}`
  * `/config/{env}/env`
  * `/config/{env}/env/export`
  * `/config/{env}/files`
  * `/config/ui`

### 6.2 Authentication

HTTP Basic Auth is optional and controlled purely by environment variables.

* If both are set:

  ```bash
  export AUTH_USERNAME="myuser"
  export AUTH_PASSWORD="mypassword"
  ```

  then Basic Auth is **required** for all endpoints (including `/ui`).

* If one or both are missing:

  * Basic Auth is **disabled** (anyone can access the server).

These env vars are **not** stored anywhere; they are only used in memory.

---

## 7. HTML UI (`/ui`)

A dark-themed UI is available at:

* `GET /ui` (or `${base_path}/ui` if you use a prefix)

Features:

* List of configured **environments** (`dev`, `test`, `ref`, `prod`, …).
* For the selected environment:
  * **Repo URL**
  * **Branch**
  * **Subpath**
  * **Workdir**
  * **Last commit hash**
  * **Commit date**
* Preview of:
  * Effective env map as **JSON** (for that environment).
  * Effective env map as **shell exports**.
  * List of files in the Git repo (under `subpath`), with:
    * On-click **templated preview** of the file.
* Copy-to-clipboard icons for:
  * Env JSON.
  * Env exports.
  * Templated file preview.

Authentication:

* Protected by the same Basic Auth mechanism:
  * username: `AUTH_USERNAME`
  * password: `AUTH_PASSWORD`

The UI is static HTML + Fomantic UI, with values injected at runtime via a JSON metadata blob.

---

## 8. Logging

Logging is implemented with [`tracing`](https://crates.io/crates/tracing) and `tracing-subscriber`:

* Default level is `info`.
* You can control it with `RUST_LOG`, for example:

```bash
RUST_LOG=info ./secure-config-server --config=config.yaml
RUST_LOG=debug ./secure-config-server --config=config.yaml
RUST_LOG=secure_config_server=debug,info ./secure-config-server --config=config.yaml
```

Git operations are logged at `info` (start) and `warn`/`error` on failure. Handlers log at `warn`/`error` for invalid requests and internal errors.

---

## 9. Running locally

Prerequisites:

* A recent stable Rust toolchain (`cargo`).
* `git` installed and available on `PATH`.

### 9.1 Prepare a local Git repo

```bash
mkdir -p ~/dev/config-server-proxy.config.repo
cd ~/dev/config-server-proxy.config.repo
git init
git checkout -b main

cat > config-client-dev.yml <<'EOF'
demo:
  message: "DEV ENV – Hello from MAIN branch (dev profile) {{FOO}}->{{BAR}}"
  number: 42
EOF

git add config-client-dev.yml
git commit -m "Initial config"
```

Optional: create a second branch:

```bash
git checkout -b release
cat > config-client-dev.yml <<'EOF'
demo:
  message: "DEV ENV – Hello from RELEASE branch (dev profile)"
EOF
git commit -am "Release config"
git checkout main
```

### 9.2 Single-instance example

`config.yaml`:

```yaml
http:
  bind_addr: "127.0.0.1:8899"
  base_path: "/"

env_from_process: true

git:
  repo_url: "file:///Users/you/dev/config-server-proxy.config.repo"
  branch: "main"
  workdir: "/Users/you/dev/config-server-proxy.config.wrk"
  refresh_interval_secs: 30
```

Run:

```bash
export AUTH_USERNAME=myuser
export AUTH_PASSWORD=mypass
export FOO=foo
export BAR=bar

cargo run -- --config=config.yaml
```

Test:

```bash
# env name is "default"
curl -s -u myuser:mypass   http://127.0.0.1:8899/default/config-client/dev | jq
```

### 9.3 Multi-tenant example

`config.yaml`:

```yaml
http:
  bind_addr: "127.0.0.1:8899"
  base_path: "/"

env_from_process: true
env_file: "/app/config/common.env"

environments:
  dev:
    git:
      repo_url: "file:///Users/you/dev/config-server-proxy.config.repo"
      branch: "main"
      workdir: "/Users/you/dev/config-server-proxy.config.wrk/dev"
      subpath: "dev"
      refresh_interval_secs: 30
    env_file: "/app/config/dev.env"

  test:
    git:
      repo_url: "file:///Users/you/dev/config-server-proxy.config.repo"
      branch: "main"
      workdir: "/Users/you/dev/config-server-proxy.config.wrk/test"
      subpath: "test"
      refresh_interval_secs: 30
    env_file: "/app/config/test.env"
```

Run:

```bash
export AUTH_USERNAME=myuser
export AUTH_PASSWORD=mypass

cargo run -- --config=config.yaml
```

Test:

```bash
# Spring-like endpoint for env "dev"
curl -s -u myuser:mypass   http://127.0.0.1:8899/dev/config-client/dev | jq

# env exports for "test"
curl -s -u myuser:mypass   http://127.0.0.1:8899/test/env/export

# templated YAML preview for env "dev"
curl -s -u myuser:mypass   http://127.0.0.1:8899/dev/file/main/config-client-dev.yml
```

### 9.4 UI

Open in browser:

```text
http://127.0.0.1:8899/ui
```

(or with your base path prefix)

---

## 10. Using with Spring Boot Config Client

You can point a Spring Boot app to this server similarly to a standard Spring Cloud Config Server, but now with an explicit environment segment.

Example for environment `dev`:

```yaml
spring:
  application:
    name: config-client

  profiles:
    active: dev

  config:
    import: "optional:configserver:http://myuser:mypass@localhost:8899/dev"
```

Or via command line:

```bash
java -jar configclient.jar   --spring.profiles.active=dev   --spring.config.import="optional:configserver:http://myuser:mypass@localhost:8899/dev"
```

Notes:

* Spring will first try the `default` profile; the server responds with 200 + empty `propertySources`, which is acceptable even without any `config-client.yml` in the repo.
* Then Spring loads `profiles='dev'` and merges `config-client-dev.yml`, `application.yml` etc., just like with the original Config Server.
* The `/dev` (or `/test`, `/prod`, …) prefix selects which environment’s Git repo / env map to use.

---

## 11. License

This project is licensed under the [MIT License](./LICENSE).
