//! Source admission uses only protected approval and read-only KMS inventory.
use super::*;
use crate::model::HelperEndpoint;
use amaterasu_module_sdk::capture::{
    SourceClass, SourceModeCapabilities, SourcePreflightRequest, SourcePreflightResponse,
};

impl<B: Backend> Controller<B> {
    pub fn preflight(
        &self,
        caller: &str,
        request: &SourcePreflightRequest,
    ) -> SourcePreflightResponse {
        if !self.settings.enabled {
            return SourcePreflightResponse::unavailable(request, "capture_disabled");
        }
        if !self.settings.capture_principals.contains(caller) {
            return SourcePreflightResponse::unavailable(request, "capture_permission_denied");
        }
        if request.source_class != SourceClass::ManagedOutput {
            return SourcePreflightResponse::unsupported(request, "source_class_unsupported");
        }
        if self.streams.len() >= MAX_STREAMS || self.opened >= MAX_LIFETIME_STREAMS {
            return SourcePreflightResponse::unavailable(request, "source_capacity");
        }
        let high_depth = cfg!(feature = "driver-read-leases") && self.settings.require_lease;
        if (request.mode.hdr || request.mode.bit_depth == Some(10)) && !high_depth {
            return SourcePreflightResponse::unsupported(request, "source_format_unsupported");
        }
        if self.settings.gpu_snapshot && !cfg!(amaterasu_vkms_gpu_snapshot) {
            return SourcePreflightResponse::unsupported(request, "gpu_snapshot_unavailable");
        }
        let helper = match self.settings.helper_endpoint() {
            Ok(helper) => helper,
            Err(error) => return SourcePreflightResponse::unavailable(request, error.code),
        };
        if matches!(helper, HelperEndpoint::ApprovedChannel(_))
            && !self.settings.authority_precheck_supported
        {
            return SourcePreflightResponse::unsupported(request, "authority_precheck_unsupported");
        }
        if let Err(error) = self.backend.preflight_helper(helper) {
            return SourcePreflightResponse::unavailable(request, error.code);
        }
        if let Err(error) = self
            .backend
            .preflight_mode(request.mode.width, request.mode.height)
        {
            return SourcePreflightResponse::unavailable(request, error.code);
        }
        if let Some(route) = request.route.as_deref() {
            let Some(output) = OutputName::parse(route) else {
                return SourcePreflightResponse::unsupported(request, "source_route_unsupported");
            };
            let binding = match self.backend.resolve(output) {
                Ok(binding) => binding,
                Err(error) => return SourcePreflightResponse::unavailable(request, error.code),
            };
            let requested_refresh = request.mode.refresh_millihz;
            // KMS's cached mode uses integral vrefresh. Do not invent precision
            // beyond that value; retain a one-hertz rounding tolerance.
            if binding.mode.width != request.mode.width
                || binding.mode.height != request.mode.height
                || binding
                    .mode
                    .refresh_hz
                    .saturating_mul(1000)
                    .abs_diff(requested_refresh)
                    > 500
            {
                return SourcePreflightResponse::unavailable(request, "source_mode_changed");
            }
        } else if let Err(error) = self.backend.discover() {
            // A disconnected connector pool is enough for a future managed
            // output. Never require the display to exist before its creation.
            return SourcePreflightResponse::unavailable(request, error.code);
        }
        SourcePreflightResponse::ready(
            request,
            vec![SourceModeCapabilities {
                source_class: SourceClass::ManagedOutput,
                min_width: 1,
                max_width: 8192,
                min_height: 1,
                max_height: 8192,
                min_refresh_millihz: 1000,
                max_refresh_millihz: 1_000_000,
                sdr: true,
                hdr: high_depth,
                bit_depths: if high_depth { vec![8, 10] } else { vec![8] },
                format_open: false,
            }],
        )
    }
}
