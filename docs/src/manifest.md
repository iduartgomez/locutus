# The Manifest Format

The `locutus.toml` file for each application/contract is called its _manifest_. It is written in the [TOML](https://toml.io/) format. Manifest files consist of the following sections:

- [[contract]](./manifest.md#the-contract-section) — Defines a contract.
  - [type](./manifest.md#the-type-field) — Contract type.
  - [lang](./manifest.md#the-lang-field) — Contract source language.
  - [output_dir](./manifest.md#the-output-field) — Output path for build artifacts.
- [[webapp]](./manifest.md#the-contract-section) — Configuration for web application containers.
- [[state]](./manifest.md#the-state-section) — Defines a web application.

## The `[contract]` section

### The `type` field

The type of the contract being packaged. Currently the following types are supported:

- `standard`, the default type, it can be ellided. This is just a standard [contract](./glossary.md#contract).
- `webapp`, a web app [container contract](./glossary.md#container-contract). Additionally to the container contract the web application source will be compiled and packaged as the state of the contract.

### The `lang` field

The programming language in which the contract is written. If specified the build tool will compile the
contract. Currently only Rust is supported.

### The `output_dir` field

An optional path to the output directory for the build artifacts. If not set the output will be written to
the relative directory `./build/locutus` from the manifest file directory.

## The `[webapp]` section

An optional section, only specified in case of `webapp` contracts.

### The `lang` field

```toml
[webapp]
...
lang =  "typescript"
```

The programming language in which the web application is written. Currently the following languages are supported:

- `typescript`, requires [npm](https://www.npmjs.com/) installed.
- `javascript`, requires [npm](https://www.npmjs.com/) installed.

### The `metadata` field

```toml
[webapp]
...
metadata =  "/path/to/metadata/file"
```

An optional path to the metadata for the webapp, if not set the metadata will be empty.

### The `[webapp.typescript]` options section

Optional section specified in case of the the `typescript` lang.

The following fields are supported:

```toml
[webapp.typescript]
webpack =  true
```

- `webpack` — if set webpack will be used when packaging the contract state.

### The `[webapp.javascript]` options section

Optional section specified in case of the the `javascript` lang.

The following fields are supported:

```toml
[webapp.javascript]
webpack =  true
```

- `webpack` — if set webpack will be used when packaging the contract state.

### The `[webapp.state-sources]` options section

### The `[webapp.dependencies]` section

## The `[state]` section
