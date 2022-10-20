use std::collections::HashSet;

use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use turbo_tasks::{primitives::StringVc, ValueToString};
use turbo_tasks_fs::FileSystemPathVc;
use turbopack_core::{
    chunk::ChunkingContextVc,
    introspect::{
        asset::IntrospectableAssetVc, Introspectable, IntrospectableChildrenVc, IntrospectableVc,
    },
};
use turbopack_dev_server::source::{
    asset_graph::AssetGraphContentSourceVc,
    conditional::ConditionalContentSourceVc,
    lazy_instatiated::{GetContentSource, GetContentSourceVc, LazyInstantiatedContentSource},
    ContentSource, ContentSourceData, ContentSourceDataFilter, ContentSourceDataVary,
    ContentSourceResult, ContentSourceResultVc, ContentSourceVc,
};
use turbopack_ecmascript::{chunk::EcmascriptChunkPlaceablesVc, EcmascriptModuleAssetVc};

use super::{external_asset_entrypoints, get_intermediate_asset, render_static, RenderData};
use crate::path_regex::PathRegexVc;

/// Trait that allows to get the entry module for rendering something in Node.js
#[turbo_tasks::value_trait]
pub trait NodeRenderer {
    fn module(&self) -> EcmascriptModuleAssetVc;
}

/// Creates a content source that renders something in Node.js with the passed
/// `renderer` when it matches a `path_regex`. Once rendered it serves
/// all assets referenced by the `renderer` that are within the `server_root`.
/// It needs a temporary directory (`intermediate_output_path`) to place file
/// for Node.js execution during rendering. The `chunking_context` should emit
/// to this directory.
#[turbo_tasks::function]
pub fn create_node_rendered_source(
    server_root: FileSystemPathVc,
    path_regex: PathRegexVc,
    renderer: NodeRendererVc,
    chunking_context: ChunkingContextVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    intermediate_output_path: FileSystemPathVc,
) -> ContentSourceVc {
    let source = NodeRenderContentSource {
        server_root,
        path_regex,
        renderer,
        chunking_context,
        runtime_entries,
        intermediate_output_path,
    }
    .cell();
    ConditionalContentSourceVc::new(
        source.into(),
        LazyInstantiatedContentSource {
            get_source: source.as_get_content_source(),
        }
        .cell()
        .into(),
    )
    .into()
}

/// see [create_node_rendered_source]
#[turbo_tasks::value]
struct NodeRenderContentSource {
    server_root: FileSystemPathVc,
    path_regex: PathRegexVc,
    renderer: NodeRendererVc,
    chunking_context: ChunkingContextVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    intermediate_output_path: FileSystemPathVc,
}

impl NodeRenderContentSource {
    /// Checks if a path matches the regular expression
    async fn is_matching_path(&self, path: &str) -> Result<bool> {
        // TODO(alexkirsz) This should probably not happen here.
        if path.starts_with('_') {
            return Ok(false);
        }
        Ok(self.path_regex.await?.is_match(path))
    }

    /// Matches a path with the regular expression and returns a JSON object
    /// with the named captures
    async fn get_matches(&self, path: &str) -> Result<Option<IndexMap<String, String>>> {
        // TODO(alexkirsz) This should probably not happen here.
        if path.starts_with('_') {
            return Ok(None);
        }
        Ok(self.path_regex.await?.get_matches(path))
    }
}

#[turbo_tasks::value_impl]
impl GetContentSource for NodeRenderContentSource {
    /// Returns the [ContentSource] that serves all referenced external
    /// assets. This is wrapped into [LazyInstantiatedContentSource].
    #[turbo_tasks::function]
    fn content_source(&self) -> ContentSourceVc {
        AssetGraphContentSourceVc::new_lazy_multiple(
            self.server_root,
            external_asset_entrypoints(
                self.renderer.module(),
                self.runtime_entries,
                self.chunking_context,
                self.intermediate_output_path,
            ),
        )
        .into()
    }
}

#[turbo_tasks::value_impl]
impl ContentSource for NodeRenderContentSource {
    #[turbo_tasks::function]
    async fn get(
        self_vc: NodeRenderContentSourceVc,
        path: &str,
        data: turbo_tasks::Value<ContentSourceData>,
    ) -> Result<ContentSourceResultVc> {
        let this = self_vc.await?;
        if this.is_matching_path(path).await? {
            if let Some(params) = this.get_matches(path).await? {
                if let Some(headers) = &data.headers {
                    if headers
                        .get("accept")
                        .map(|value| value.contains("html"))
                        .unwrap_or_default()
                        || headers.contains_key("__rsc__")
                    {
                        if data.method.is_some() && data.url.is_some() {
                            if let Some(query) = &data.query {
                                return Ok(ContentSourceResult::Static(
                                    render_static(
                                        this.server_root.join(path),
                                        this.renderer.module(),
                                        this.runtime_entries,
                                        this.chunking_context,
                                        this.intermediate_output_path,
                                        RenderData {
                                            params,
                                            method: data.method.clone().ok_or_else(|| {
                                                anyhow!("method needs to be provided")
                                            })?,
                                            url: data.url.clone().ok_or_else(|| {
                                                anyhow!("url needs to be provided")
                                            })?,
                                            query: query.clone(),
                                            headers: headers.clone(),
                                            path: format!("/{path}"),
                                        }
                                        .cell(),
                                    )
                                    .into(),
                                )
                                .cell());
                            }
                        }
                        return Ok(ContentSourceResult::NeedData {
                            source: self_vc.into(),
                            path: path.to_string(),
                            vary: ContentSourceDataVary {
                                method: true,
                                url: true,
                                headers: Some(ContentSourceDataFilter::All),
                                query: Some(ContentSourceDataFilter::All),
                                ..Default::default()
                            },
                        }
                        .cell());
                    }
                } else {
                    return Ok(ContentSourceResult::NeedData {
                        source: self_vc.into(),
                        path: path.to_string(),
                        vary: ContentSourceDataVary {
                            headers: Some(ContentSourceDataFilter::Subset(HashSet::from([
                                "accept".to_string(),
                                "__rsc__".to_string(),
                            ]))),
                            ..Default::default()
                        },
                    }
                    .cell());
                }
            }
        }
        Ok(ContentSourceResult::NotFound.cell())
    }
}

#[turbo_tasks::function]
fn introspectable_type() -> StringVc {
    StringVc::cell("node render content source".to_string())
}

#[turbo_tasks::value_impl]
impl Introspectable for NodeRenderContentSource {
    #[turbo_tasks::function]
    fn ty(&self) -> StringVc {
        introspectable_type()
    }

    #[turbo_tasks::function]
    fn title(&self) -> StringVc {
        self.path_regex.to_string()
    }

    #[turbo_tasks::function]
    fn children(&self) -> IntrospectableChildrenVc {
        IntrospectableChildrenVc::cell(HashSet::from([
            (
                StringVc::cell("module".to_string()),
                IntrospectableAssetVc::new(self.renderer.module().into()),
            ),
            (
                StringVc::cell("intermediate asset".to_string()),
                IntrospectableAssetVc::new(get_intermediate_asset(
                    self.renderer.module(),
                    self.runtime_entries,
                    self.chunking_context,
                    self.intermediate_output_path,
                )),
            ),
        ]))
    }
}