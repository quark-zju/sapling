/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use async_trait::async_trait;
use blobstore::Blobstore;
use blobstore::Loadable;
use blobstore::LoadableError;
use context::CoreContext;
use sorted_vector_map::SortedVectorMap;

use crate::MPathElement;
use crate::ThriftConvert;
use crate::blob::Blob;
use crate::blob::BlobstoreValue;
use crate::blob::TestManifestBlob;
use crate::thrift;
use crate::typed_hash::IdContext;
use crate::typed_hash::TestManifestId;
use crate::typed_hash::TestManifestIdContext;

/// A manifest type intended only to be used in tests. It contains
/// only the file names and the maximum basename length of all files
/// in each directory.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TestManifest {
    pub subentries: SortedVectorMap<MPathElement, TestManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestManifestEntry {
    File,
    Directory(TestManifestDirectory),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TestManifestDirectory {
    pub id: TestManifestId,
    pub max_basename_length: u64,
}

impl ThriftConvert for TestManifestDirectory {
    const NAME: &'static str = "TestManifestDirectory";
    type Thrift = thrift::test_manifest::TestManifestDirectory;

    fn from_thrift(t: Self::Thrift) -> Result<Self> {
        Ok(Self {
            id: ThriftConvert::from_thrift(t.id)?,
            max_basename_length: t.max_basename_length as u64,
        })
    }

    fn into_thrift(self) -> Self::Thrift {
        thrift::test_manifest::TestManifestDirectory {
            id: self.id.into_thrift(),
            max_basename_length: self.max_basename_length as i64,
        }
    }
}

#[async_trait]
impl Loadable for TestManifestDirectory {
    type Value = TestManifest;

    async fn load<'a, B: Blobstore>(
        &'a self,
        ctx: &'a CoreContext,
        blobstore: &'a B,
    ) -> Result<Self::Value, LoadableError> {
        self.id.load(ctx, blobstore).await
    }
}

impl ThriftConvert for TestManifestEntry {
    const NAME: &'static str = "TestManifestEntry";
    type Thrift = thrift::test_manifest::TestManifestEntry;

    fn from_thrift(t: Self::Thrift) -> Result<Self> {
        Ok(match t {
            thrift::test_manifest::TestManifestEntry::file(_) => Self::File,
            thrift::test_manifest::TestManifestEntry::directory(dir) => {
                Self::Directory(ThriftConvert::from_thrift(dir)?)
            }
            thrift::test_manifest::TestManifestEntry::UnknownField(variant) => {
                anyhow::bail!("Unknown variant: {}", variant)
            }
        })
    }

    fn into_thrift(self) -> Self::Thrift {
        match self {
            Self::File => thrift::test_manifest::TestManifestEntry::file(
                thrift::test_manifest::TestManifestFile {},
            ),
            Self::Directory(dir) => {
                thrift::test_manifest::TestManifestEntry::directory(dir.into_thrift())
            }
        }
    }
}

impl ThriftConvert for TestManifest {
    const NAME: &'static str = "TestManifest";
    type Thrift = thrift::test_manifest::TestManifest;
    fn from_thrift(t: Self::Thrift) -> Result<Self> {
        Ok(Self {
            subentries: t
                .subentries
                .into_iter()
                .map(|(element, entry)| {
                    Ok((
                        MPathElement::from_thrift(element)?,
                        ThriftConvert::from_thrift(entry)?,
                    ))
                })
                .collect::<Result<_>>()?,
        })
    }

    fn into_thrift(self) -> Self::Thrift {
        thrift::test_manifest::TestManifest {
            subentries: self
                .subentries
                .into_iter()
                .map(|(element, entry)| (element.into_thrift(), entry.into_thrift()))
                .collect(),
        }
    }
}

impl BlobstoreValue for TestManifest {
    type Key = TestManifestId;

    fn into_blob(self) -> TestManifestBlob {
        let data = self.into_bytes();
        let id = TestManifestIdContext::id_from_data(&data);
        Blob::new(id, data)
    }

    fn from_blob(blob: Blob<Self::Key>) -> Result<Self> {
        Self::from_bytes(blob.data())
    }
}

impl TestManifest {
    pub fn empty() -> Self {
        Self {
            subentries: Default::default(),
        }
    }

    pub fn from_subentries(
        subentries: impl Iterator<Item = (MPathElement, TestManifestEntry)>,
    ) -> Self {
        Self {
            subentries: subentries.collect(),
        }
    }

    pub fn lookup(&self, path_element: &MPathElement) -> Option<&TestManifestEntry> {
        self.subentries.get(path_element)
    }

    pub fn list(&self) -> impl Iterator<Item = (&MPathElement, &TestManifestEntry)> {
        self.subentries.iter()
    }
}
