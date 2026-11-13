use std::path::Path;
use std::sync::Arc;

use pnp::{Manifest, Resolution};
use tsrs_core::{ResolutionMode, P};
use tsrs_module::{
    DefaultResolver, DiagAndArgs, ResolutionData, ResolvedModule, ResolvedProjectReference,
    ResolvedTypeReferenceDirective, Resolver, ResolverOptions,
};

pub(super) struct PnpResolver<'h> {
    default: DefaultResolver<'h>,
    manifest: Arc<Manifest>,
    fs: &'h dyn tsrs_vfs::FS,
}

impl<'h> PnpResolver<'h> {
    pub fn new(options: ResolverOptions<'h>, manifest: Arc<Manifest>) -> Self {
        let fs = options.host.fs();
        Self {
            default: tsrs_module::new_resolver(options),
            manifest,
            fs,
        }
    }

    // DefaultResolver walks ancestors after a failed lookup. PnP must not admit
    // a dependency from a different workspace's node_modules view on that walk.
    fn permits(&self, request: &str, issuer: &Path, resolved: &str) -> bool {
        if request.starts_with(['.', '/', '#']) || Path::new(request).is_absolute() {
            return true;
        }
        let types = tsrs_module::get_types_package_name(request);
        let resolved = self.fs.realpath(resolved);
        let issuer = self.fs.realpath(&issuer.to_string_lossy());
        // An unresolved relative issuer has no PnP owner. Keep the default
        // resolver's OS result rather than passing it to find_locator.
        if !Path::new(&issuer).is_absolute() {
            return true;
        }
        [request, &types].iter().any(|request| {
            match pnp::resolve_to_unqualified_via_manifest(
                &self.manifest,
                request,
                Path::new(&issuer),
            ) {
                Ok(Resolution::Resolved(root, _)) => Path::new(&resolved).starts_with(root),
                Ok(Resolution::Skipped) => true,
                Err(_) => false,
            }
        })
    }

    fn filter_module(
        &self,
        request: &str,
        issuer: &Path,
        result: P<ResolvedModule>,
    ) -> P<ResolvedModule> {
        if !result.is_external_library_import
            || self.permits(request, issuer, result.resolved_file_name)
        {
            result
        } else {
            P::new(ResolvedModule::default())
        }
    }
}

impl Resolver for PnpResolver<'_> {
    // @codescene(disable:"Excess Number of Function Arguments") Signature fixed by upstream tsrs_module::Resolver.
    fn resolve_module_name(
        &self,
        module_name: &str,
        containing_file: &str,
        resolution_mode: ResolutionMode,
        redirected_reference: Option<&dyn ResolvedProjectReference>,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        let (result, trace) = self.default.resolve_module_name(
            module_name,
            containing_file,
            resolution_mode,
            redirected_reference,
        )?;
        Ok((
            self.filter_module(module_name, Path::new(containing_file), result),
            trace,
        ))
    }

    fn resolve_module_name_from_directory(
        &self,
        module_name: &str,
        containing_directory: &str,
        resolution_mode: ResolutionMode,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String> {
        let (result, trace) = self.default.resolve_module_name_from_directory(
            module_name,
            containing_directory,
            resolution_mode,
        )?;
        let issuer = Path::new(containing_directory).join("package.json");
        Ok((self.filter_module(module_name, &issuer, result), trace))
    }

    // @codescene(disable:"Excess Number of Function Arguments") Signature fixed by upstream tsrs_module::Resolver.
    fn resolve_type_reference_directive(
        &self,
        name: &str,
        containing_file: &str,
        resolution_mode: ResolutionMode,
        redirected_reference: Option<&dyn ResolvedProjectReference>,
    ) -> (P<ResolvedTypeReferenceDirective>, Vec<DiagAndArgs>) {
        let (result, trace) = Resolver::resolve_type_reference_directive(
            &self.default,
            name,
            containing_file,
            resolution_mode,
            redirected_reference,
        );
        if !result.is_external_library_import
            || self.permits(name, Path::new(containing_file), result.resolved_file_name)
        {
            (result, trace)
        } else {
            (P::new(ResolvedTypeReferenceDirective::default()), trace)
        }
    }

    fn get_resolution_data(&self) -> P<ResolutionData> {
        self.default.get_resolution_data()
    }
}
