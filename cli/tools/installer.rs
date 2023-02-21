// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use crate::args::resolve_no_prompt;
use crate::args::CaData;
use crate::args::ConfigFlag;
use crate::args::Flags;
use crate::args::InstallFlags;
use crate::args::TypeCheckMode;
use crate::http_util::HttpClient;
use crate::proc_state::ProcState;
use crate::util::fs::canonicalize_path_maybe_not_exists;

use deno_core::anyhow::Context;
use deno_core::error::generic_error;
use deno_core::error::AnyError;
use deno_core::resolve_url_or_path;
use deno_core::url::Url;
use deno_graph::npm::NpmPackageReqReference;
use log::Level;
use once_cell::sync::Lazy;
use regex::Regex;
use regex::RegexBuilder;
use std::env;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[cfg(not(windows))]
use std::os::unix::fs::PermissionsExt;

static EXEC_NAME_RE: Lazy<Regex> = Lazy::new(|| {
  RegexBuilder::new(r"^[a-z][\w-]*$")
    .case_insensitive(true)
    .build()
    .unwrap()
});

fn validate_name(exec_name: &str) -> Result<(), AnyError> {
  if EXEC_NAME_RE.is_match(exec_name) {
    Ok(())
  } else {
    Err(generic_error(format!(
      "Invalid executable name: {exec_name}"
    )))
  }
}

#[cfg(windows)]
/// On Windows, 2 files are generated.
/// One compatible with cmd & powershell with a .cmd extension
/// A second compatible with git bash / MINGW64
/// Generate batch script to satisfy that.
fn generate_executable_file(shim_data: &ShimData) -> Result<(), AnyError> {
  let args: Vec<String> =
    shim_data.args.iter().map(|c| format!("\"{c}\"")).collect();
  let template = format!(
    "% generated by deno install %\n@deno {} %*\n",
    args
      .iter()
      .map(|arg| arg.replace('%', "%%"))
      .collect::<Vec<_>>()
      .join(" ")
  );
  let mut file = File::create(&shim_data.file_path)?;
  file.write_all(template.as_bytes())?;

  // write file for bash
  // create filepath without extensions
  let template = format!(
    r#"#!/bin/sh
# generated by deno install
deno {} "$@"
"#,
    args.join(" "),
  );
  let mut file = File::create(shim_data.file_path.with_extension(""))?;
  file.write_all(template.as_bytes())?;
  Ok(())
}

#[cfg(not(windows))]
fn generate_executable_file(shim_data: &ShimData) -> Result<(), AnyError> {
  use shell_escape::escape;
  let args: Vec<String> = shim_data
    .args
    .iter()
    .map(|c| escape(c.into()).into_owned())
    .collect();
  let template = format!(
    r#"#!/bin/sh
# generated by deno install
exec deno {} "$@"
"#,
    args.join(" "),
  );
  let mut file = File::create(&shim_data.file_path)?;
  file.write_all(template.as_bytes())?;
  let _metadata = fs::metadata(&shim_data.file_path)?;
  let mut permissions = _metadata.permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&shim_data.file_path, permissions)?;
  Ok(())
}

fn get_installer_root() -> Result<PathBuf, io::Error> {
  if let Ok(env_dir) = env::var("DENO_INSTALL_ROOT") {
    if !env_dir.is_empty() {
      return canonicalize_path_maybe_not_exists(&PathBuf::from(env_dir));
    }
  }
  // Note: on Windows, the $HOME environment variable may be set by users or by
  // third party software, but it is non-standard and should not be relied upon.
  let home_env_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
  let mut home_path =
    env::var_os(home_env_var)
      .map(PathBuf::from)
      .ok_or_else(|| {
        io::Error::new(
          io::ErrorKind::NotFound,
          format!("${home_env_var} is not defined"),
        )
      })?;
  home_path.push(".deno");
  Ok(home_path)
}

pub async fn infer_name_from_url(url: &Url) -> Option<String> {
  // If there's an absolute url with no path, eg. https://my-cli.com
  // perform a request, and see if it redirects another file instead.
  let mut url = url.clone();

  if url.path() == "/" {
    let client = HttpClient::new(None, None).unwrap();
    if let Ok(res) = client.get_redirected_response(url.clone()).await {
      url = res.url().clone();
    }
  }

  if let Ok(npm_ref) = NpmPackageReqReference::from_specifier(&url) {
    if let Some(sub_path) = npm_ref.sub_path {
      if !sub_path.contains('/') {
        return Some(sub_path);
      }
    }
    if !npm_ref.req.name.contains('/') {
      return Some(npm_ref.req.name);
    }
    return None;
  }

  let path = PathBuf::from(url.path());
  let mut stem = match path.file_stem() {
    Some(stem) => stem.to_string_lossy().to_string(),
    None => return None,
  };
  if stem == "main" || stem == "mod" || stem == "index" || stem == "cli" {
    if let Some(parent_name) = path.parent().and_then(|p| p.file_name()) {
      stem = parent_name.to_string_lossy().to_string();
    }
  }

  // if atmark symbol appears in the index other than 0 (e.g. `foo@bar`) we use
  // the former part as the inferred name because the latter part is most likely
  // a version number.
  match stem.find('@') {
    Some(at_index) if at_index > 0 => {
      stem = stem.split_at(at_index).0.to_string();
    }
    _ => {}
  }

  Some(stem)
}

pub fn uninstall(name: String, root: Option<PathBuf>) -> Result<(), AnyError> {
  let root = if let Some(root) = root {
    canonicalize_path_maybe_not_exists(&root)?
  } else {
    get_installer_root()?
  };
  let installation_dir = root.join("bin");

  // ensure directory exists
  if let Ok(metadata) = fs::metadata(&installation_dir) {
    if !metadata.is_dir() {
      return Err(generic_error("Installation path is not a directory"));
    }
  }

  let file_path = installation_dir.join(&name);

  let mut removed = false;

  if file_path.exists() {
    fs::remove_file(&file_path)?;
    log::info!("deleted {}", file_path.to_string_lossy());
    removed = true
  };

  if cfg!(windows) {
    let file_path = file_path.with_extension("cmd");
    if file_path.exists() {
      fs::remove_file(&file_path)?;
      log::info!("deleted {}", file_path.to_string_lossy());
      removed = true
    }
  }

  if !removed {
    return Err(generic_error(format!("No installation found for {name}")));
  }

  // There might be some extra files to delete
  // Note: tsconfig.json is legacy. We renamed it to deno.json.
  // Remove cleaning it up after January 2024
  for ext in ["tsconfig.json", "deno.json", "lock.json"] {
    let file_path = file_path.with_extension(ext);
    if file_path.exists() {
      fs::remove_file(&file_path)?;
      log::info!("deleted {}", file_path.to_string_lossy());
    }
  }

  log::info!("✅ Successfully uninstalled {}", name);
  Ok(())
}

pub async fn install_command(
  flags: Flags,
  install_flags: InstallFlags,
) -> Result<(), AnyError> {
  // ensure the module is cached
  ProcState::build(flags.clone())
    .await?
    .load_and_type_check_files(&[install_flags.module_url.clone()])
    .await?;

  // create the install shim
  create_install_shim(flags, install_flags).await
}

async fn create_install_shim(
  flags: Flags,
  install_flags: InstallFlags,
) -> Result<(), AnyError> {
  let shim_data = resolve_shim_data(&flags, &install_flags).await?;

  // ensure directory exists
  if let Ok(metadata) = fs::metadata(&shim_data.installation_dir) {
    if !metadata.is_dir() {
      return Err(generic_error("Installation path is not a directory"));
    }
  } else {
    fs::create_dir_all(&shim_data.installation_dir)?;
  };

  if shim_data.file_path.exists() && !install_flags.force {
    return Err(generic_error(
      "Existing installation found. Aborting (Use -f to overwrite).",
    ));
  };

  generate_executable_file(&shim_data)?;
  for (path, contents) in shim_data.extra_files {
    fs::write(path, contents)?;
  }

  log::info!("✅ Successfully installed {}", shim_data.name);
  log::info!("{}", shim_data.file_path.display());
  if cfg!(windows) {
    let display_path = shim_data.file_path.with_extension("");
    log::info!("{} (shell)", display_path.display());
  }
  let installation_dir_str = shim_data.installation_dir.to_string_lossy();

  if !is_in_path(&shim_data.installation_dir) {
    log::info!("ℹ️  Add {} to PATH", installation_dir_str);
    if cfg!(windows) {
      log::info!("    set PATH=%PATH%;{}", installation_dir_str);
    } else {
      log::info!("    export PATH=\"{}:$PATH\"", installation_dir_str);
    }
  }

  Ok(())
}

struct ShimData {
  name: String,
  installation_dir: PathBuf,
  file_path: PathBuf,
  args: Vec<String>,
  extra_files: Vec<(PathBuf, String)>,
}

async fn resolve_shim_data(
  flags: &Flags,
  install_flags: &InstallFlags,
) -> Result<ShimData, AnyError> {
  let root = if let Some(root) = &install_flags.root {
    canonicalize_path_maybe_not_exists(root)?
  } else {
    get_installer_root()?
  };
  let installation_dir = root.join("bin");

  // Check if module_url is remote
  let module_url = resolve_url_or_path(&install_flags.module_url)?;

  let name = if install_flags.name.is_some() {
    install_flags.name.clone()
  } else {
    infer_name_from_url(&module_url).await
  };

  let name = match name {
    Some(name) => name,
    None => return Err(generic_error(
      "An executable name was not provided. One could not be inferred from the URL. Aborting.",
    )),
  };

  validate_name(name.as_str())?;
  let mut file_path = installation_dir.join(&name);

  if cfg!(windows) {
    file_path = file_path.with_extension("cmd");
  }

  let mut extra_files: Vec<(PathBuf, String)> = vec![];

  let mut executable_args = vec!["run".to_string()];
  executable_args.extend_from_slice(&flags.to_permission_args());
  if let Some(url) = flags.location.as_ref() {
    executable_args.push("--location".to_string());
    executable_args.push(url.to_string());
  }
  if let Some(CaData::File(ca_file)) = &flags.ca_data {
    executable_args.push("--cert".to_string());
    executable_args.push(ca_file.to_owned())
  }
  if let Some(log_level) = flags.log_level {
    if log_level == Level::Error {
      executable_args.push("--quiet".to_string());
    } else {
      executable_args.push("--log-level".to_string());
      let log_level = match log_level {
        Level::Debug => "debug",
        Level::Info => "info",
        _ => {
          return Err(generic_error(format!("invalid log level {log_level}")))
        }
      };
      executable_args.push(log_level.to_string());
    }
  }

  // we should avoid a default branch here to ensure we continue to cover any
  // changes to this flag.
  match flags.type_check_mode {
    TypeCheckMode::All => executable_args.push("--check=all".to_string()),
    TypeCheckMode::None => {}
    TypeCheckMode::Local => executable_args.push("--check".to_string()),
  }

  if flags.unstable {
    executable_args.push("--unstable".to_string());
  }

  if flags.no_remote {
    executable_args.push("--no-remote".to_string());
  }

  if flags.no_npm {
    executable_args.push("--no-npm".to_string());
  }

  if flags.lock_write {
    executable_args.push("--lock-write".to_string());
  }

  if flags.cached_only {
    executable_args.push("--cached-only".to_string());
  }

  if resolve_no_prompt(flags) {
    executable_args.push("--no-prompt".to_string());
  }

  if !flags.v8_flags.is_empty() {
    executable_args.push(format!("--v8-flags={}", flags.v8_flags.join(",")));
  }

  if let Some(seed) = flags.seed {
    executable_args.push("--seed".to_string());
    executable_args.push(seed.to_string());
  }

  if let Some(inspect) = flags.inspect {
    executable_args.push(format!("--inspect={inspect}"));
  }

  if let Some(inspect_brk) = flags.inspect_brk {
    executable_args.push(format!("--inspect-brk={inspect_brk}"));
  }

  if let Some(import_map_path) = &flags.import_map_path {
    let import_map_url = resolve_url_or_path(import_map_path)?;
    executable_args.push("--import-map".to_string());
    executable_args.push(import_map_url.to_string());
  }

  if let ConfigFlag::Path(config_path) = &flags.config_flag {
    let copy_path = get_hidden_file_with_ext(&file_path, "deno.json");
    executable_args.push("--config".to_string());
    executable_args.push(copy_path.to_str().unwrap().to_string());
    extra_files.push((
      copy_path,
      fs::read_to_string(config_path)
        .with_context(|| format!("error reading {config_path}"))?,
    ));
  } else {
    executable_args.push("--no-config".to_string());
  }

  if flags.no_lock {
    executable_args.push("--no-lock".to_string());
  } else if flags.lock.is_some()
    // always use a lockfile for an npm entrypoint unless --no-lock
    || NpmPackageReqReference::from_specifier(&module_url).is_ok()
  {
    let copy_path = get_hidden_file_with_ext(&file_path, "lock.json");
    executable_args.push("--lock".to_string());
    executable_args.push(copy_path.to_str().unwrap().to_string());

    if let Some(lock_path) = &flags.lock {
      extra_files.push((
        copy_path,
        fs::read_to_string(lock_path)
          .with_context(|| format!("error reading {}", lock_path.display()))?,
      ));
    } else {
      // Provide an empty lockfile so that this overwrites any existing lockfile
      // from a previous installation. This will get populated on first run.
      extra_files.push((copy_path, "{}".to_string()));
    }
  }

  executable_args.push(module_url.to_string());
  executable_args.extend_from_slice(&install_flags.args);

  Ok(ShimData {
    name,
    installation_dir,
    file_path,
    args: executable_args,
    extra_files,
  })
}

fn get_hidden_file_with_ext(file_path: &Path, ext: &str) -> PathBuf {
  // use a dot file to prevent the file from showing up in some
  // users shell auto-complete since this directory is on the PATH
  file_path
    .with_file_name(format!(
      ".{}",
      file_path.file_name().unwrap().to_string_lossy()
    ))
    .with_extension(ext)
}

fn is_in_path(dir: &Path) -> bool {
  if let Some(paths) = env::var_os("PATH") {
    for p in env::split_paths(&paths) {
      if *dir == p {
        return true;
      }
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;

  use crate::args::ConfigFlag;
  use crate::util::fs::canonicalize_path;
  use std::process::Command;
  use test_util::testdata_path;
  use test_util::TempDir;

  #[tokio::test]
  async fn install_infer_name_from_url() {
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/server.ts").unwrap()
      )
      .await,
      Some("server".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/main.ts").unwrap()
      )
      .await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/mod.ts").unwrap()
      )
      .await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/index.ts").unwrap()
      )
      .await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/cli.ts").unwrap()
      )
      .await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("https://example.com/main.ts").unwrap())
        .await,
      Some("main".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("https://example.com").unwrap()).await,
      None
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///abc/server.ts").unwrap()).await,
      Some("server".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///abc/main.ts").unwrap()).await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///main.ts").unwrap()).await,
      Some("main".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///").unwrap()).await,
      None
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc@0.1.0").unwrap()
      )
      .await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc@0.1.0/main.ts").unwrap()
      )
      .await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc@def@ghi").unwrap()
      )
      .await,
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("https://example.com/@abc.ts").unwrap())
        .await,
      Some("@abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/@abc/mod.ts").unwrap()
      )
      .await,
      Some("@abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///@abc.ts").unwrap()).await,
      Some("@abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///@abc/cli.ts").unwrap()).await,
      Some("@abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("npm:cowsay@1.2/cowthink").unwrap())
        .await,
      Some("cowthink".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("npm:cowsay@1.2/cowthink/test").unwrap())
        .await,
      Some("cowsay".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("npm:cowsay@1.2").unwrap()).await,
      Some("cowsay".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("npm:@types/node@1.2").unwrap()).await,
      None
    );
  }

  #[tokio::test]
  async fn install_unstable() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    create_install_shim(
      Flags {
        unstable: true,
        ..Flags::default()
      },
      InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: false,
      },
    )
    .await
    .unwrap();

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());

    let content = fs::read_to_string(file_path).unwrap();
    if cfg!(windows) {
      assert!(content.contains(
        r#""run" "--unstable" "--no-config" "http://localhost:4545/echo_server.ts""#
      ));
    } else {
      assert!(content.contains(
        r#"run --unstable --no-config 'http://localhost:4545/echo_server.ts'"#
      ));
    }
  }

  #[tokio::test]
  async fn install_inferred_name() {
    let shim_data = resolve_shim_data(
      &Flags::default(),
      &InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec![],
        name: None,
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(shim_data.name, "echo_server");
    assert_eq!(
      shim_data.args,
      vec!["run", "--no-config", "http://localhost:4545/echo_server.ts",]
    );
  }

  #[tokio::test]
  async fn install_inferred_name_from_parent() {
    let shim_data = resolve_shim_data(
      &Flags::default(),
      &InstallFlags {
        module_url: "http://localhost:4545/subdir/main.ts".to_string(),
        args: vec![],
        name: None,
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(shim_data.name, "subdir");
    assert_eq!(
      shim_data.args,
      vec!["run", "--no-config", "http://localhost:4545/subdir/main.ts",]
    );
  }

  #[tokio::test]
  async fn install_inferred_name_after_redirect_for_no_path_url() {
    let _http_server_guard = test_util::http_server();
    let shim_data = resolve_shim_data(
      &Flags::default(),
      &InstallFlags {
        module_url: "http://localhost:4550/?redirect_to=/subdir/redirects/a.ts"
          .to_string(),
        args: vec![],
        name: None,
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(shim_data.name, "a");
    assert_eq!(
      shim_data.args,
      vec![
        "run",
        "--no-config",
        "http://localhost:4550/?redirect_to=/subdir/redirects/a.ts",
      ]
    );
  }

  #[tokio::test]
  async fn install_custom_dir_option() {
    let shim_data = resolve_shim_data(
      &Flags::default(),
      &InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(shim_data.name, "echo_test");
    assert_eq!(
      shim_data.args,
      vec!["run", "--no-config", "http://localhost:4545/echo_server.ts",]
    );
  }

  #[tokio::test]
  async fn install_with_flags() {
    let shim_data = resolve_shim_data(
      &Flags {
        allow_net: Some(vec![]),
        allow_read: Some(vec![]),
        type_check_mode: TypeCheckMode::None,
        log_level: Some(Level::Error),
        ..Flags::default()
      },
      &InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec!["--foobar".to_string()],
        name: Some("echo_test".to_string()),
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(shim_data.name, "echo_test");
    assert_eq!(
      shim_data.args,
      vec![
        "run",
        "--allow-read",
        "--allow-net",
        "--quiet",
        "--no-config",
        "http://localhost:4545/echo_server.ts",
        "--foobar",
      ]
    );
  }

  #[tokio::test]
  async fn install_prompt() {
    let shim_data = resolve_shim_data(
      &Flags {
        no_prompt: true,
        ..Flags::default()
      },
      &InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(
      shim_data.args,
      vec![
        "run",
        "--no-prompt",
        "--no-config",
        "http://localhost:4545/echo_server.ts",
      ]
    );
  }

  #[tokio::test]
  async fn install_allow_all() {
    let shim_data = resolve_shim_data(
      &Flags {
        allow_all: true,
        ..Flags::default()
      },
      &InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(
      shim_data.args,
      vec![
        "run",
        "--allow-all",
        "--no-config",
        "http://localhost:4545/echo_server.ts",
      ]
    );
  }

  #[tokio::test]
  async fn install_npm_lockfile_default() {
    let temp_dir = canonicalize_path(&env::temp_dir()).unwrap();
    let shim_data = resolve_shim_data(
      &Flags {
        allow_all: true,
        ..Flags::default()
      },
      &InstallFlags {
        module_url: "npm:cowsay".to_string(),
        args: vec![],
        name: None,
        root: Some(temp_dir.clone()),
        force: false,
      },
    )
    .await
    .unwrap();

    let lock_path = temp_dir.join("bin").join(".cowsay.lock.json");
    assert_eq!(
      shim_data.args,
      vec![
        "run",
        "--allow-all",
        "--no-config",
        "--lock",
        &lock_path.to_string_lossy(),
        "npm:cowsay"
      ]
    );
    assert_eq!(shim_data.extra_files, vec![(lock_path, "{}".to_string())]);
  }

  #[tokio::test]
  async fn install_npm_no_lock() {
    let shim_data = resolve_shim_data(
      &Flags {
        allow_all: true,
        no_lock: true,
        ..Flags::default()
      },
      &InstallFlags {
        module_url: "npm:cowsay".to_string(),
        args: vec![],
        name: None,
        root: Some(env::temp_dir()),
        force: false,
      },
    )
    .await
    .unwrap();

    assert_eq!(
      shim_data.args,
      vec![
        "run",
        "--allow-all",
        "--no-config",
        "--no-lock",
        "npm:cowsay"
      ]
    );
    assert_eq!(shim_data.extra_files, vec![]);
  }

  #[tokio::test]
  async fn install_local_module() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let local_module = env::current_dir().unwrap().join("echo_server.ts");
    let local_module_url = Url::from_file_path(&local_module).unwrap();
    let local_module_str = local_module.to_string_lossy();

    create_install_shim(
      Flags::default(),
      InstallFlags {
        module_url: local_module_str.to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: false,
      },
    )
    .await
    .unwrap();

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    assert!(content.contains(&local_module_url.to_string()));
  }

  #[tokio::test]
  async fn install_force() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    create_install_shim(
      Flags::default(),
      InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: false,
      },
    )
    .await
    .unwrap();

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }
    assert!(file_path.exists());

    // No force. Install failed.
    let no_force_result = create_install_shim(
      Flags::default(),
      InstallFlags {
        module_url: "http://localhost:4545/cat.ts".to_string(), // using a different URL
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: false,
      },
    )
    .await;
    assert!(no_force_result.is_err());
    assert!(no_force_result
      .unwrap_err()
      .to_string()
      .contains("Existing installation found"));
    // Assert not modified
    let file_content = fs::read_to_string(&file_path).unwrap();
    assert!(file_content.contains("echo_server.ts"));

    // Force. Install success.
    let force_result = create_install_shim(
      Flags::default(),
      InstallFlags {
        module_url: "http://localhost:4545/cat.ts".to_string(), // using a different URL
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: true,
      },
    )
    .await;
    assert!(force_result.is_ok());
    // Assert modified
    let file_content_2 = fs::read_to_string(&file_path).unwrap();
    assert!(file_content_2.contains("cat.ts"));
  }

  #[tokio::test]
  async fn install_with_config() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    let config_file_path = temp_dir.path().join("test_tsconfig.json");
    let config = "{}";
    let mut config_file = File::create(&config_file_path).unwrap();
    let result = config_file.write_all(config.as_bytes());
    assert!(result.is_ok());

    let result = create_install_shim(
      Flags {
        config_flag: ConfigFlag::Path(
          config_file_path.to_string_lossy().to_string(),
        ),
        ..Flags::default()
      },
      InstallFlags {
        module_url: "http://localhost:4545/cat.ts".to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: true,
      },
    )
    .await;
    assert!(result.is_ok());

    let config_file_name = ".echo_test.deno.json";

    let file_path = bin_dir.join(config_file_name);
    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    assert!(content == "{}");
  }

  // TODO: enable on Windows after fixing batch escaping
  #[cfg(not(windows))]
  #[tokio::test]
  async fn install_shell_escaping() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    create_install_shim(
      Flags::default(),
      InstallFlags {
        module_url: "http://localhost:4545/echo_server.ts".to_string(),
        args: vec!["\"".to_string()],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: false,
      },
    )
    .await
    .unwrap();

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    if cfg!(windows) {
      // TODO: see comment above this test
    } else {
      assert!(content.contains(
        r#"run --no-config 'http://localhost:4545/echo_server.ts' '"'"#
      ));
    }
  }

  #[tokio::test]
  async fn install_unicode() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let unicode_dir = temp_dir.path().join("Magnús");
    std::fs::create_dir(&unicode_dir).unwrap();
    let local_module = unicode_dir.join("echo_server.ts");
    let local_module_str = local_module.to_string_lossy();
    std::fs::write(&local_module, "// Some JavaScript I guess").unwrap();

    create_install_shim(
      Flags::default(),
      InstallFlags {
        module_url: local_module_str.to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: false,
      },
    )
    .await
    .unwrap();

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    // We need to actually run it to make sure the URL is interpreted correctly
    let status = Command::new(file_path)
      .env_clear()
      // use the deno binary in the target directory
      .env("PATH", test_util::target_dir())
      .spawn()
      .unwrap()
      .wait()
      .unwrap();
    assert!(status.success());
  }

  #[tokio::test]
  async fn install_with_import_map() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    let import_map_path = temp_dir.path().join("import_map.json");
    let import_map_url = Url::from_file_path(&import_map_path).unwrap();
    let import_map = "{ \"imports\": {} }";
    let mut import_map_file = File::create(&import_map_path).unwrap();
    let result = import_map_file.write_all(import_map.as_bytes());
    assert!(result.is_ok());

    let result = create_install_shim(
      Flags {
        import_map_path: Some(import_map_path.to_string_lossy().to_string()),
        ..Flags::default()
      },
      InstallFlags {
        module_url: "http://localhost:4545/cat.ts".to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: true,
      },
    )
    .await;
    assert!(result.is_ok());

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }
    assert!(file_path.exists());

    let mut expected_string = format!(
      "--import-map '{import_map_url}' --no-config 'http://localhost:4545/cat.ts'"
    );
    if cfg!(windows) {
      expected_string = format!(
        "\"--import-map\" \"{import_map_url}\" \"--no-config\" \"http://localhost:4545/cat.ts\""
      );
    }

    let content = fs::read_to_string(file_path).unwrap();
    assert!(content.contains(&expected_string));
  }

  // Regression test for https://github.com/denoland/deno/issues/10556.
  #[tokio::test]
  async fn install_file_url() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    let module_path = fs::canonicalize(testdata_path().join("cat.ts")).unwrap();
    let file_module_string =
      Url::from_file_path(module_path).unwrap().to_string();
    assert!(file_module_string.starts_with("file:///"));

    let result = create_install_shim(
      Flags::default(),
      InstallFlags {
        module_url: file_module_string.to_string(),
        args: vec![],
        name: Some("echo_test".to_string()),
        root: Some(temp_dir.path().to_path_buf()),
        force: true,
      },
    )
    .await;
    assert!(result.is_ok());

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }
    assert!(file_path.exists());

    let mut expected_string =
      format!("run --no-config '{}'", &file_module_string);
    if cfg!(windows) {
      expected_string =
        format!("\"run\" \"--no-config\" \"{}\"", &file_module_string);
    }

    let content = fs::read_to_string(file_path).unwrap();
    assert!(content.contains(&expected_string));
  }

  #[test]
  fn uninstall_basic() {
    let temp_dir = TempDir::new();
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    let mut file_path = bin_dir.join("echo_test");
    File::create(&file_path).unwrap();
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
      File::create(&file_path).unwrap();
    }

    // create extra files
    {
      let file_path = file_path.with_extension("deno.json");
      File::create(file_path).unwrap();
    }
    {
      // legacy tsconfig.json, make sure it's cleaned up for now
      let file_path = file_path.with_extension("tsconfig.json");
      File::create(file_path).unwrap();
    }
    {
      let file_path = file_path.with_extension("lock.json");
      File::create(file_path).unwrap();
    }

    uninstall("echo_test".to_string(), Some(temp_dir.path().to_path_buf()))
      .unwrap();

    assert!(!file_path.exists());
    assert!(!file_path.with_extension("tsconfig.json").exists());
    assert!(!file_path.with_extension("deno.json").exists());
    assert!(!file_path.with_extension("lock.json").exists());

    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
      assert!(!file_path.exists());
    }
  }
}
