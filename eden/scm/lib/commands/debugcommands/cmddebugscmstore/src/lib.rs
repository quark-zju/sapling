/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::io::Write;

use async_runtime::block_on;
use async_runtime::stream_to_iter as block_on_stream;
use clidispatch::ReqCtx;
use clidispatch::abort;
use clidispatch::abort_if;
use clidispatch::errors;
use cmdutil::ConfigSet;
use cmdutil::Error;
use cmdutil::IO;
use cmdutil::Result;
use cmdutil::define_flags;
use manifest::FileMetadata;
use manifest::FsNodeMetadata;
use manifest::Manifest;
use repo::repo::Repo;
use revisionstore::scmstore::FileAttributes;
use revisionstore::scmstore::file_to_async_key_stream;
use revisionstore::scmstore::tree::types::TreeAttributes;
use serde::de::Deserialize;
use serde::de::value;
use serde::de::value::StringDeserializer;
use storemodel::TreeStore;
use types::FetchContext;
use types::Key;
use types::RepoPathBuf;
use types::fetch_mode::FetchMode;

define_flags! {
    pub struct DebugScmStoreOpts {
        /// Fetch mode (file or tree)
        mode: String,

        /// Input file containing keys to fetch (hgid,path separated by newlines)
        requests_file: Option<String>,

        /// Choose fetch mode (e.g. local_only or allow_remote)
        fetch_mode: Option<String>,

        /// Only fetch AUX data (don't request file/tree content).
        aux_only: bool,

        /// Only fetch pure file content (allows request to go to CAS).
        pure_content: bool,

        /// Request tree parents.
        tree_parents: bool,

        /// Run in storemodel (using storemodel traits). This is what eden uses.
        store_model: bool,

        /// Revision for positional file paths.
        #[short('r')]
        #[argtype("REV")]
        rev: Option<String>,

        #[args]
        args: Vec<String>,
    }
}

#[derive(PartialEq)]
enum FetchType {
    File,
    Tree,
}

pub fn run(ctx: ReqCtx<DebugScmStoreOpts>, repo: &Repo) -> Result<u8> {
    let mode = match ctx.opts.mode.as_ref() {
        "file" => FetchType::File,
        "tree" => FetchType::Tree,
        _ => return Err(errors::Abort("'mode' must be one of 'file' or 'tree'".into()).into()),
    };

    abort_if!(
        ctx.opts.requests_file.is_some() == ctx.opts.rev.is_some(),
        "must specify exactly one of --rev or --path"
    );

    let keys: Vec<Key> = if let Some(path) = ctx.opts.requests_file {
        block_on_stream(block_on(file_to_async_key_stream(path.into()))?).collect()
    } else {
        let wc = repo.working_copy()?;
        let commit =
            repo.resolve_commit(Some(&wc.read().treestate().lock()), &ctx.opts.rev.unwrap())?;
        let manifest = repo.tree_resolver()?.get(&commit)?;
        ctx.opts
            .args
            .into_iter()
            .map(|path| {
                let path = RepoPathBuf::from_string(path)?;
                match manifest.get(&path)? {
                    None => abort!("path {path} not in manifest"),
                    Some(FsNodeMetadata::Directory(hgid)) => {
                        if mode == FetchType::File {
                            abort!("path {path} is a directory");
                        }
                        Ok(Key::new(path, hgid.unwrap()))
                    }
                    Some(FsNodeMetadata::File(FileMetadata { hgid, .. })) => {
                        if mode == FetchType::Tree {
                            abort!("path {path} is a file");
                        }
                        Ok(Key::new(path, hgid))
                    }
                }
            })
            .collect::<Result<_>>()?
    };

    // We downloaded trees above when handling args. Let's make a
    // fresh repo to recreate the cache state before we were invoked.
    let fresh_repo = Repo::load_with_config(repo.path(), ConfigSet::wrap(repo.config().clone()))?;
    // And reset counters so tests don't see counters from above arg handling.
    metrics::Registry::global().reset();

    let fctx = FetchContext::new(FetchMode::deserialize(
        StringDeserializer::<value::Error>::new(
            ctx.opts
                .fetch_mode
                .unwrap_or_else(|| "LOCAL | REMOTE".to_string()),
        ),
    )?);

    match mode {
        FetchType::File => fetch_files(
            fctx,
            &ctx.core.io,
            &fresh_repo,
            keys,
            ctx.opts.aux_only,
            ctx.opts.pure_content,
        )?,
        FetchType::Tree => fetch_trees(
            fctx,
            &ctx.core.io,
            &fresh_repo,
            keys,
            ctx.opts.store_model,
            ctx.opts.tree_parents,
            ctx.opts.aux_only,
        )?,
    }

    Ok(0)
}

fn fetch_files(
    fctx: FetchContext,
    io: &IO,
    repo: &Repo,
    keys: Vec<Key>,
    aux_only: bool,
    pure_content: bool,
) -> Result<()> {
    repo.file_store()?;
    let store = repo.file_scm_store().unwrap();

    let mut stdout = io.output();

    let mut fetch_and_display_successes =
        |keys: Vec<Key>, attrs: FileAttributes| -> HashMap<Key, Error> {
            let fetch_result = store.fetch(fctx.clone(), keys, attrs);

            let (found, missing, _errors) = fetch_result.consume();
            for (_, file) in found.into_iter() {
                let _ = write!(stdout, "Successfully fetched file: {:#?}\n", file);
            }

            missing
        };

    let mut missing = fetch_and_display_successes(
        keys,
        FileAttributes {
            pure_content: !aux_only,
            content_header: !aux_only && !pure_content,
            aux_data: true,
        },
    );

    if !aux_only {
        // Maybe we failed because only one of content or aux data is available.
        // The API doesn't let us say "aux data if present", so try each separately.
        missing = fetch_and_display_successes(
            missing.into_keys().collect(),
            FileAttributes {
                pure_content: true,
                content_header: !pure_content,
                aux_data: false,
            },
        );
        missing = fetch_and_display_successes(
            missing.into_keys().collect(),
            FileAttributes {
                pure_content: false,
                content_header: false,
                aux_data: true,
            },
        );
    }

    for (key, err) in missing.into_iter() {
        write!(stdout, "Failed to fetch file: {key:#?}\nError: {err:?}\n")?;
    }

    Ok(())
}

fn fetch_trees(
    fctx: FetchContext,
    io: &IO,
    repo: &Repo,
    keys: Vec<Key>,
    store_model: bool,
    tree_parents: bool,
    aux_only: bool,
) -> Result<()> {
    repo.tree_store()?;
    let store = repo.tree_scm_store().unwrap();

    let mut stdout = io.output();

    let mut attrs = if aux_only {
        TreeAttributes::AUX_DATA
    } else {
        TreeAttributes::CONTENT
    };
    if tree_parents {
        attrs |= TreeAttributes::PARENTS;
    }

    if store_model {
        for tree in store.get_tree_iter(fctx, keys)? {
            let (key, tree) = tree?;

            writeln!(stdout, "Tree '{}' entries", key.path)?;
            for entry in tree.iter()? {
                writeln!(stdout, "  {:?}", entry?)?;
            }

            writeln!(stdout, "Tree '{}' file aux", key.path)?;
            let mut file_aux = tree.file_aux_iter()?.collect::<Result<Vec<_>>>()?;
            file_aux.sort_by(|a, b| a.0.cmp(&b.0));
            for entry in file_aux {
                writeln!(stdout, "  {:?}", entry)?;
            }
        }
    } else {
        let fetch_result = store.fetch_batch(fctx, keys.into_iter(), attrs);

        let (found, missing, _errors) = fetch_result.consume();
        for complete in found.into_iter() {
            write!(stdout, "Successfully fetched tree: {:#?}\n", complete)?;
        }
        for incomplete in missing.into_iter() {
            write!(stdout, "Failed to fetch tree: {:#?}\n", incomplete)?;
        }
    }

    Ok(())
}

pub fn aliases() -> &'static str {
    "debugscmstore"
}

pub fn doc() -> &'static str {
    "test file and tree fetching using scmstore"
}

pub fn synopsis() -> Option<&'static str> {
    None
}

pub fn enable_cas() -> bool {
    true
}
