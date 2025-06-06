/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;
use std::path::Path;
use std::sync::Arc;

use dag::Set;
use dag::Vertex;
use dag::delegate;
use futures::stream::BoxStream;
use minibytes::Bytes;
use storemodel::SerializationFormat;

use crate::AppendCommits;
use crate::DescribeBackend;
use crate::HgCommit;
use crate::OnDiskCommits;
use crate::ParentlessHgCommit;
use crate::ReadCommitText;
use crate::Result;
use crate::RevlogCommits;
use crate::StreamCommitText;
use crate::StripCommits;

/// Segmented Changelog + Revlog.
///
/// Use segmented changelog for the commit graph algorithms and IdMap.
/// Use revlog for fallback commit messages. Double writes to revlog.
pub struct DoubleWriteCommits {
    revlog: RevlogCommits,
    commits: OnDiskCommits,
    format: SerializationFormat,
}

impl DoubleWriteCommits {
    pub fn new(
        revlog_dir: &Path,
        dag_path: &Path,
        commits_path: &Path,
        format: SerializationFormat,
    ) -> Result<Self> {
        let commits = OnDiskCommits::new(dag_path, commits_path, format)?;
        let revlog = RevlogCommits::new(revlog_dir, format)?;
        Ok(Self {
            revlog,
            commits,
            format,
        })
    }
}

#[async_trait::async_trait]
impl AppendCommits for DoubleWriteCommits {
    async fn add_commits(&mut self, commits: &[HgCommit]) -> Result<()> {
        self.revlog.add_commits(commits).await?;
        self.commits.add_commits(commits).await?;
        Ok(())
    }

    async fn flush(&mut self, master_heads: &[Vertex]) -> Result<()> {
        self.revlog.flush(master_heads).await?;
        self.commits.flush(master_heads).await?;
        Ok(())
    }

    async fn flush_commit_data(&mut self) -> Result<()> {
        self.revlog.flush_commit_data().await?;
        self.commits.flush_commit_data().await?;
        Ok(())
    }

    async fn update_virtual_nodes(&mut self, wdir_parents: Vec<Vertex>) -> Result<()> {
        self.commits.update_virtual_nodes(wdir_parents).await
    }
}

#[async_trait::async_trait]
impl ReadCommitText for DoubleWriteCommits {
    async fn get_commit_raw_text(&self, vertex: &Vertex) -> Result<Option<Bytes>> {
        match self.commits.get_commit_raw_text(vertex).await {
            Ok(None) => self.revlog.get_commit_raw_text(vertex).await,
            result => result,
        }
    }

    fn to_dyn_read_commit_text(&self) -> Arc<dyn ReadCommitText + Send + Sync> {
        self.revlog.to_dyn_read_commit_text()
    }

    fn format(&self) -> SerializationFormat {
        self.format
    }
}

impl StreamCommitText for DoubleWriteCommits {
    fn stream_commit_raw_text(
        &self,
        stream: BoxStream<'static, anyhow::Result<Vertex>>,
    ) -> Result<BoxStream<'static, anyhow::Result<ParentlessHgCommit>>> {
        self.revlog.stream_commit_raw_text(stream)
    }
}

#[async_trait::async_trait]
impl StripCommits for DoubleWriteCommits {
    async fn strip_commits(&mut self, set: Set) -> Result<()> {
        self.revlog.strip_commits(set.clone()).await?;
        self.commits.strip_commits(set).await?;
        Ok(())
    }
}

delegate!(CheckIntegrity | IdConvert | IdMapSnapshot | PrefixLookup | DagAlgorithm, DoubleWriteCommits => self.commits);

impl DescribeBackend for DoubleWriteCommits {
    fn algorithm_backend(&self) -> &'static str {
        "segments"
    }

    fn describe_backend(&self) -> String {
        format!(
            r#"Backend (doublewrite):
  Local:
    Segments + IdMap: {}
    Zstore: {}
    Revlog + Nodemap: {}
Feature Providers:
  Commit Graph Algorithms:
    Segments
  Commit Hash / Rev Lookup:
    IdMap
  Commit Data (user, message):
    Zstore (incomplete)
    Revlog
"#,
            self.commits.dag_path.display(),
            self.commits.commits_path.display(),
            self.revlog.dir.join("00changelog.{i,d,nodemap}").display(),
        )
    }

    fn explain_internals(&self, w: &mut dyn io::Write) -> io::Result<()> {
        self.commits.explain_internals(w)
    }
}
