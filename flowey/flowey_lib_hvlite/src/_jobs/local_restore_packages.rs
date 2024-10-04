// Copyright (C) Microsoft Corporation. All rights reserved.

use crate::download_lxutil::LxutilArch;
use crate::download_uefi_mu_msvm::MuMsvmArch;
use crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request{
        pub arch: CommonArch,
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::init_openvmm_magicpath_protoc::Node>();
        ctx.import::<crate::init_openvmm_magicpath_lxutil::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::init_openvmm_magicpath_uefi_mu_msvm::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut deps = vec![ctx.reqv(crate::init_openvmm_magicpath_protoc::Request)];

        for req in &requests {
            match req.arch {
                CommonArch::X86_64 => {
                    let (openhcl_sysroot_read, openhcl_sysroot_write) = ctx.new_var();
                    ctx.req(crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                        arch: OpenvmmSysrootArch::X64,
                        path: openhcl_sysroot_write,
                    });
                    deps.extend_from_slice(&[
                        openhcl_sysroot_read.into_side_effect(),
                        ctx.reqv(|done| crate::init_openvmm_magicpath_lxutil::Request {
                            arch: LxutilArch::X86_64,
                            done,
                        }),
                        ctx.reqv(|done| crate::init_openvmm_magicpath_uefi_mu_msvm::Request {
                            arch: MuMsvmArch::X86_64,
                            done,
                        }),
                    ]);
                }
                CommonArch::Aarch64 => {
                    let (openhcl_sysroot_read, openhcl_sysroot_write) = ctx.new_var();
                    ctx.req(crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                        arch: OpenvmmSysrootArch::Aarch64,
                        path: openhcl_sysroot_write,
                    });
                    deps.extend_from_slice(&[
                        openhcl_sysroot_read.into_side_effect(),
                        ctx.reqv(|done| crate::init_openvmm_magicpath_lxutil::Request {
                            arch: LxutilArch::Aarch64,
                            done,
                        }),
                        ctx.reqv(|done| crate::init_openvmm_magicpath_uefi_mu_msvm::Request {
                            arch: MuMsvmArch::Aarch64,
                            done,
                        }),
                    ]);
                }
            }
        }

        ctx.emit_side_effect_step(deps, requests.into_iter().map(|x| x.done));

        Ok(())
    }
}