use crate::credentials::load_credential;
use crate::git_state::current_branch;
use crate::process::{RunOptions, git, run};
use crate::protocol::{Catalog, CatalogProject};
use crate::state::{Config, LocalProject, load_config_raw};
use crate::transaction::reconcile_workspace;
use anyhow::{Result, bail};
use base64::Engine;
use reqwest::blocking::Client;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use url::Url;

pub fn authorization_env(token: Option<&str>) -> BTreeMap<OsString, OsString> {
    let Some(token) = token else {
        return BTreeMap::new();
    };
    BTreeMap::from([
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()),
        (
            "GIT_CONFIG_VALUE_0".into(),
            format!(
                "Authorization: Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("resync:{token}"))
            )
            .into(),
        ),
    ])
}

pub fn resolved_config() -> Result<Config> {
    let mut config = load_config_raw()?;
    let origin = config.active_provider.clone();
    config.server = origin.clone();
    config.token = origin
        .as_deref()
        .map(load_credential)
        .transpose()?
        .flatten()
        .or(config.token);
    Ok(config)
}

pub fn fetch_catalog(config: &Config) -> Result<Catalog> {
    let server = config
        .server
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("run `resync login SERVER TOKEN` first"))?;
    let token = config
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("run `resync login SERVER TOKEN` first"))?;
    let url = Url::parse(server)?.join("/v1/catalog")?;
    let response = Client::new()
        .get(url)
        .bearer_auth(token)
        .header("accept", "application/json")
        .send()?;
    let status = response.status();
    let text = response.text()?;
    if !status.is_success() {
        bail!(
            "catalog request failed ({}): {}",
            status.as_u16(),
            text.chars().take(500).collect::<String>()
        );
    }
    Catalog::from_value(serde_json::from_str(&text)?)
}

fn refs(project_path: &Path, prefix: &str) -> Result<BTreeMap<String, String>> {
    let options = RunOptions {
        allow_failure: true,
        ..RunOptions::default()
    };
    let result = git(
        project_path,
        [
            "for-each-ref",
            "--format=%(refname:strip=2)\t%(objectname)",
            prefix,
        ],
        options,
    )?;
    if result.code != 0 || result.stdout.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    Ok(result
        .stdout
        .trim()
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(name, oid)| (name.into(), oid.into()))
        .collect())
}

pub fn local_heads(project_path: &Path) -> Result<BTreeMap<String, String>> {
    refs(project_path, "refs/heads")
}

pub fn remote_heads(project_path: &Path, remote_name: &str) -> Result<BTreeMap<String, String>> {
    Ok(refs(project_path, &format!("refs/remotes/{remote_name}"))?
        .into_iter()
        .filter_map(|(name, oid)| {
            name.strip_prefix(&format!("{remote_name}/"))
                .filter(|suffix| *suffix != "HEAD")
                .map(|suffix| (suffix.into(), oid))
        })
        .collect())
}

fn is_ancestor(project_path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let options = RunOptions {
        allow_failure: true,
        ..RunOptions::default()
    };
    Ok(git(
        project_path,
        ["merge-base", "--is-ancestor", ancestor, descendant],
        options,
    )?
    .code
        == 0)
}

pub fn fetch_remote(
    project: &LocalProject,
    token: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let refspec = format!("+refs/heads/*:refs/remotes/{}/*", project.remote_name);
    let options = RunOptions {
        env: authorization_env(token),
        allow_failure: true,
        ..RunOptions::default()
    };
    let result = git(
        &project.local_path,
        [
            "fetch",
            "--prune",
            "--no-tags",
            &project.remote_name,
            &refspec,
        ],
        options,
    )?;
    if result.code != 0 {
        if result
            .stderr
            .to_lowercase()
            .contains("couldn't find remote ref")
        {
            return Ok(BTreeMap::new());
        }
        bail!(
            "{}",
            if result.stderr.trim().is_empty() {
                "Git fetch failed"
            } else {
                result.stderr.trim()
            }
        );
    }
    remote_heads(&project.local_path, &project.remote_name)
}

fn configure_remote(destination: &Path, remote_name: &str, remote_url: &str) -> Result<()> {
    let options = RunOptions {
        allow_failure: true,
        ..RunOptions::default()
    };
    let current = git(destination, ["remote", "get-url", remote_name], options)?;
    if current.code == 0 {
        git(
            destination,
            ["remote", "set-url", remote_name, remote_url],
            RunOptions::default(),
        )?;
    } else {
        git(
            destination,
            ["remote", "add", remote_name, remote_url],
            RunOptions::default(),
        )?;
    }
    git(
        destination,
        [
            "config",
            &format!("remote.{remote_name}.fetch"),
            &format!("+refs/heads/*:refs/remotes/{remote_name}/*"),
        ],
        RunOptions::default(),
    )?;
    Ok(())
}

fn adopt_exact_snapshot(
    destination: &Path,
    catalog_project: &CatalogProject,
    token: Option<&str>,
    remote_name: &str,
) -> Result<()> {
    let metadata = destination.join(".git");
    if metadata.exists() {
        bail!(
            "{} contains Git metadata but is not a valid worktree",
            destination.display()
        );
    }
    git(
        destination,
        [
            "init",
            &format!("--initial-branch={}", catalog_project.default_branch),
        ],
        RunOptions::default(),
    )?;
    let result = (|| -> Result<()> {
        configure_remote(destination, remote_name, &catalog_project.remote_url)?;
        let refspec = format!("+refs/heads/*:refs/remotes/{remote_name}/*");
        git(
            destination,
            ["fetch", "--prune", "--no-tags", remote_name, &refspec],
            RunOptions {
                env: authorization_env(token),
                ..RunOptions::default()
            },
        )?;
        let revisions = git(
            destination,
            ["rev-list", "--topo-order", "--all"],
            RunOptions::default(),
        )?;
        let baseline = revisions
            .stdout
            .lines()
            .find(|oid| {
                if !git(
                    destination,
                    ["read-tree", oid],
                    RunOptions {
                        allow_failure: true,
                        ..RunOptions::default()
                    },
                )
                .is_ok_and(|result| result.code == 0)
                {
                    return false;
                }
                git(
                    destination,
                    ["diff", "--quiet", "--", "."],
                    RunOptions {
                        allow_failure: true,
                        ..RunOptions::default()
                    },
                )
                .is_ok_and(|result| result.code == 0)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} does not exactly match the tracked files of any hosted commit; refusing to adopt it",
                    destination.display()
                )
            })?;
        let default_head = catalog_project
            .advertised_heads
            .iter()
            .find(|head| head.name == catalog_project.default_branch);
        let mut branch = match default_head {
            Some(head) if is_ancestor(destination, baseline, &head.oid)? => {
                Some(head.name.as_str())
            }
            _ => None,
        };
        if branch.is_none() {
            for head in &catalog_project.advertised_heads {
                if is_ancestor(destination, baseline, &head.oid)? {
                    branch = Some(head.name.as_str());
                    break;
                }
            }
        }
        let branch = branch.ok_or_else(|| {
            anyhow::anyhow!(
                "matching commit {baseline} is not reachable from an advertised hosted branch"
            )
        })?;
        let reference = format!("refs/heads/{branch}");
        git(
            destination,
            ["symbolic-ref", "HEAD", &reference],
            RunOptions::default(),
        )?;
        git(
            destination,
            ["update-ref", &reference, baseline],
            RunOptions::default(),
        )?;
        git(
            destination,
            ["reset", "--mixed", baseline],
            RunOptions::default(),
        )?;
        git(
            destination,
            ["config", &format!("branch.{branch}.remote"), remote_name],
            RunOptions::default(),
        )?;
        git(
            destination,
            [
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{branch}"),
            ],
            RunOptions::default(),
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(metadata);
    }
    result
}

pub fn materialize_project(
    catalog_project: &CatalogProject,
    local_path: &Path,
    token: Option<&str>,
    remote_name: &str,
) -> Result<LocalProject> {
    let destination = if local_path.is_absolute() {
        local_path.to_owned()
    } else {
        std::env::current_dir()?.join(local_path)
    };
    let advertised: BTreeMap<String, String> = catalog_project
        .advertised_heads
        .iter()
        .map(|head| (head.name.clone(), head.oid.clone()))
        .collect();
    if !destination.exists() {
        if !advertised.is_empty() {
            fs::create_dir_all(destination.parent().unwrap_or_else(|| Path::new(".")))?;
            let options = RunOptions {
                cwd: destination.parent().map(Path::to_owned),
                env: authorization_env(token),
                ..RunOptions::default()
            };
            run(
                "git",
                [
                    "clone",
                    "--origin",
                    remote_name,
                    &catalog_project.remote_url,
                    destination.to_string_lossy().as_ref(),
                ],
                options,
            )?;
            let options = RunOptions {
                allow_failure: true,
                ..RunOptions::default()
            };
            if git(&destination, ["rev-parse", "--verify", "HEAD"], options)?.code != 0 {
                let branch = if advertised.contains_key(&catalog_project.default_branch) {
                    catalog_project.default_branch.clone()
                } else {
                    advertised.keys().next().cloned().unwrap()
                };
                git(
                    &destination,
                    [
                        "switch",
                        "-c",
                        &branch,
                        "--track",
                        &format!("{remote_name}/{branch}"),
                    ],
                    RunOptions::default(),
                )?;
            }
        } else {
            fs::create_dir_all(&destination)?;
            git(
                &destination,
                [
                    "init",
                    &format!("--initial-branch={}", catalog_project.default_branch),
                ],
                RunOptions::default(),
            )?;
            git(
                &destination,
                ["remote", "add", remote_name, &catalog_project.remote_url],
                RunOptions::default(),
            )?;
        }
    } else {
        let options = RunOptions {
            allow_failure: true,
            ..RunOptions::default()
        };
        if git(
            &destination,
            ["rev-parse", "--is-inside-work-tree"],
            options,
        )?
        .code
            != 0
        {
            if advertised.is_empty() {
                bail!("{} exists but is not a Git worktree", destination.display());
            }
            adopt_exact_snapshot(&destination, catalog_project, token, remote_name)?;
        }
    }
    configure_remote(&destination, remote_name, &catalog_project.remote_url)?;
    let mut project = LocalProject {
        project_id: catalog_project.project_id.clone(),
        local_path: fs::canonicalize(&destination)?,
        remote_url: catalog_project.remote_url.clone(),
        remote_name: remote_name.into(),
        default_branch: catalog_project.default_branch.clone(),
        server_generation: catalog_project.project_generation,
        advertised_heads: BTreeMap::new(),
        last_applied_heads: BTreeMap::new(),
        active_branch: None,
        workspace_generation: 0,
        state: "CURRENT".into(),
        durability: "REMOTELY_DURABLE".into(),
        conflict: None,
        last_error: None,
        service: None,
        checkout_id: None,
        extra: BTreeMap::new(),
    };
    let mut hosted_heads = fetch_remote(&project, token)?;
    let mut checkout_heads = local_heads(&destination)?;
    let branch = current_branch(&destination).ok();
    if hosted_heads.is_empty() {
        if !checkout_heads.is_empty() {
            let options = RunOptions {
                env: authorization_env(token),
                ..RunOptions::default()
            };
            git(
                &destination,
                ["push", remote_name, "refs/heads/*:refs/heads/*"],
                options,
            )?;
            hosted_heads = fetch_remote(&project, token)?;
        }
    } else {
        let mut to_publish = Vec::new();
        for (name, local_oid) in &checkout_heads {
            let Some(remote_oid) = hosted_heads.get(name) else {
                to_publish.push(format!("refs/heads/{name}:refs/heads/{name}"));
                continue;
            };
            if local_oid == remote_oid {
                continue;
            }
            let remote_is_base = is_ancestor(&destination, remote_oid, local_oid)?;
            let local_is_base = is_ancestor(&destination, local_oid, remote_oid)?;
            if !remote_is_base && !local_is_base {
                bail!(
                    "local and hosted histories for branch {name} diverge; RepoSync will not choose a project identity or overwrite either history"
                );
            }
            if remote_is_base {
                to_publish.push(format!("refs/heads/{name}:refs/heads/{name}"));
            }
        }
        if !to_publish.is_empty() {
            let mut arguments = vec!["push".to_owned(), remote_name.to_owned()];
            arguments.extend(to_publish);
            let options = RunOptions {
                env: authorization_env(token),
                ..RunOptions::default()
            };
            git(&destination, arguments, options)?;
            hosted_heads = fetch_remote(&project, token)?;
        }
        if let Some(branch) = &branch
            && let (Some(local), Some(remote)) =
                (checkout_heads.get(branch), hosted_heads.get(branch))
            && local != remote
            && is_ancestor(&destination, local, remote)?
        {
            let result = reconcile_workspace(&destination, local, remote, None)?;
            if result.outcome != "applied" {
                bail!(
                    "joining the hosted project conflicts: {}",
                    result.detail.unwrap_or(result.outcome)
                );
            }
            checkout_heads = local_heads(&destination)?;
        }
    }
    let names: BTreeSet<String> = hosted_heads.keys().cloned().collect();
    let mut last_applied = BTreeMap::new();
    for name in names {
        let remote = &hosted_heads[&name];
        let selected = match checkout_heads.get(&name) {
            Some(local) if local != remote && is_ancestor(&destination, local, remote)? => local,
            _ => remote,
        };
        last_applied.insert(name, selected.clone());
    }
    project.advertised_heads = hosted_heads;
    project.last_applied_heads = last_applied;
    project.active_branch = branch;
    project.workspace_generation = u64::from(!project.advertised_heads.is_empty());
    Ok(project)
}
