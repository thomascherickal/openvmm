// Copyright (C) Microsoft Corporation. All rights reserved.

//! SNP VP context builder.

use super::vbs::VbsVpContext;
use crate::file_loader::HV_NUM_VTLS;
use crate::vp_context_builder::VpContextBuilder;
use crate::vp_context_builder::VpContextPageState;
use crate::vp_context_builder::VpContextState;
use hvdef::Vtl;
use igvm_defs::PAGE_SIZE_4K;
use loader::importer::BootPageAcceptance;
use loader::importer::SegmentRegister;
use loader::importer::TableRegister;
use loader::importer::X86Register;
use loader::paravisor::HCL_SECURE_VTL;
use std::fmt::Debug;
use x86defs::snp::SevSelector;
use x86defs::snp::SevVmsa;
use x86defs::X64_EFER_SVME;
use zerocopy::AsBytes;
use zerocopy::FromZeroes;

// The usage of this enum is in an outer box, so it doesn't need to box
// internally itself.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum SnpVpContext {
    None,
    Hardware(SnpHardwareContext),
    Vbs(VbsVpContext<X86Register>),
}

impl SnpVpContext {
    fn import_vp_register(&mut self, register: X86Register) {
        match self {
            SnpVpContext::None => {
                panic!("importing register to None context")
            }
            SnpVpContext::Hardware(hardware_context) => hardware_context.import_register(register),
            SnpVpContext::Vbs(vbs_context) => vbs_context.import_vp_register(register),
        }
    }

    fn vp_context_page(&self) -> anyhow::Result<u64> {
        match self {
            SnpVpContext::None => Err(anyhow::anyhow!("no vp context available")),
            SnpVpContext::Hardware(hardware_context) => hardware_context.vp_context_page(),
            SnpVpContext::Vbs(vbs_context) => vbs_context.vp_context_page(),
        }
    }

    fn set_vp_context_memory(&mut self, page_base: u64, acceptance: BootPageAcceptance) {
        match self {
            SnpVpContext::None => panic!("setting vp context memory on None context"),
            SnpVpContext::Hardware(hardware_context) => {
                hardware_context.set_vp_context_memory(page_base, acceptance)
            }
            SnpVpContext::Vbs(vbs_context) => {
                vbs_context.set_vp_context_memory(page_base, acceptance)
            }
        }
    }

    fn finalize(self) -> Vec<VpContextState> {
        match self {
            SnpVpContext::None => Vec::new(),
            SnpVpContext::Hardware(hardware_context) => hardware_context.finalize(),
            SnpVpContext::Vbs(vbs_context) => match vbs_context.finalize() {
                None => Vec::new(),
                Some(state) => vec![state],
            },
        }
    }
}

/// The interrupt injection type to use for the highest vmpl's VMSA.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum InjectionType {
    /// Normal.
    Normal,
    /// Restricted injection.
    Restricted,
}

/// A hardware SNP VP context, that is imported as a VMSA.
#[derive(Debug)]
struct SnpHardwareContext {
    /// If an assembly stub to accept the lower 1mb should be imported as page
    /// data.
    accept_lower_1mb: bool,
    /// The acceptance to import this vp context as. This must be
    /// [`BootPageAcceptance::VpContext`].
    acceptance: Option<BootPageAcceptance>,
    /// The page number to import this vp context at.
    page_number: u64,
    /// The VMSA for this VP.
    vmsa: SevVmsa,
}

impl SnpHardwareContext {
    fn new(
        vtl: Vtl,
        enlightened_uefi: bool,
        shared_gpa_boundary: u64,
        injection_type: InjectionType,
    ) -> Self {
        let mut vmsa: SevVmsa = FromZeroes::new_zeroed();

        // Fill in reset values that are needed for consistency.
        vmsa.efer = X64_EFER_SVME;

        // Fill in boilerplate fields of the vmsa
        vmsa.sev_features.set_snp(true);
        vmsa.sev_features.set_vtom(true);
        vmsa.virtual_tom = shared_gpa_boundary;
        vmsa.sev_features.set_debug_swap(true);

        if enlightened_uefi {
            // Enlightened UEFI requires SevFeatureRestrictInjection to be set, in order
            // to receive #HV interrupts.
            assert_eq!(injection_type, InjectionType::Restricted);
            vmsa.sev_features.set_restrict_injection(true);
        } else {
            // Lower VTLs like VTL0 images (UEFI) are SevFeatureAlternateInjection,
            // while VTL2 (HCL) is SevFeatureRestrictInjection
            // Additionally, set the BTB isolation and Prevent Host IBS property for
            // VTL2. VTL2 is responsible for setting this property on any additional
            // VMSAs.
            if vtl < HCL_SECURE_VTL {
                vmsa.sev_features
                    .set_alternate_injection(injection_type == InjectionType::Restricted);
            } else {
                vmsa.sev_features
                    .set_restrict_injection(injection_type == InjectionType::Restricted);
                vmsa.sev_features.set_snp_btb_isolation(true);
                vmsa.sev_features.set_prevent_host_ibs(true);
                vmsa.sev_features.set_vmsa_reg_prot(true);
                vmsa.sev_features.set_vtom(false);
                vmsa.virtual_tom = 0;
            }
        }

        // Configure the hardware reset value for XFEM.  The HCL will execute XSETBV if it needs
        // additional XSAVE support.
        vmsa.xcr0 = 0x1; // Maps to LegacyX87 bit

        SnpHardwareContext {
            accept_lower_1mb: enlightened_uefi,
            acceptance: None,
            page_number: 0,
            vmsa,
        }
    }

    fn import_register(&mut self, register: X86Register) {
        let create_vmsa_table_register = |reg: TableRegister| -> SevSelector {
            SevSelector {
                limit: reg.limit as u32,
                base: reg.base,
                ..FromZeroes::new_zeroed()
            }
        };

        let create_vmsa_segment_register = |reg: SegmentRegister| -> SevSelector {
            SevSelector {
                limit: reg.limit,
                base: reg.base,
                selector: reg.selector,
                attrib: (reg.attributes & 0xFF) | ((reg.attributes >> 4) & 0xF00),
            }
        };

        match register {
            X86Register::Gdtr(reg) => self.vmsa.gdtr = create_vmsa_table_register(reg),
            X86Register::Idtr(_) => panic!("Idtr not allowed for SNP"),
            X86Register::Ds(reg) => self.vmsa.ds = create_vmsa_segment_register(reg),
            X86Register::Es(reg) => self.vmsa.es = create_vmsa_segment_register(reg),
            X86Register::Fs(reg) => self.vmsa.fs = create_vmsa_segment_register(reg),
            X86Register::Gs(reg) => self.vmsa.gs = create_vmsa_segment_register(reg),
            X86Register::Ss(reg) => self.vmsa.ss = create_vmsa_segment_register(reg),
            X86Register::Cs(reg) => self.vmsa.cs = create_vmsa_segment_register(reg),
            X86Register::Tr(reg) => self.vmsa.tr = create_vmsa_segment_register(reg),
            X86Register::Cr0(reg) => self.vmsa.cr0 = reg,
            X86Register::Cr3(reg) => self.vmsa.cr3 = reg,
            X86Register::Cr4(reg) => self.vmsa.cr4 = reg,
            X86Register::Efer(reg) => {
                // All SEV guests require EFER.SVME for the VMSA to be valid.
                self.vmsa.efer = reg | X64_EFER_SVME;
            }
            X86Register::Pat(reg) => self.vmsa.pat = reg,
            X86Register::Rbp(reg) => self.vmsa.rbp = reg,
            X86Register::Rip(reg) => self.vmsa.rip = reg,
            X86Register::Rsi(reg) => self.vmsa.rsi = reg,
            X86Register::Rsp(_) => panic!("rsp not allowed for SNP"),
            X86Register::R8(reg) => self.vmsa.r8 = reg,
            X86Register::R9(reg) => self.vmsa.r9 = reg,
            X86Register::R10(reg) => self.vmsa.r10 = reg,
            X86Register::R11(reg) => self.vmsa.r11 = reg,
            X86Register::R12(reg) => self.vmsa.r12 = reg,
            X86Register::Rflags(_) => panic!("rflags not allowed for SNP"),

            X86Register::MtrrDefType(_)
            | X86Register::MtrrPhysBase0(_)
            | X86Register::MtrrPhysMask0(_)
            | X86Register::MtrrPhysBase1(_)
            | X86Register::MtrrPhysMask1(_)
            | X86Register::MtrrPhysBase2(_)
            | X86Register::MtrrPhysMask2(_)
            | X86Register::MtrrPhysBase3(_)
            | X86Register::MtrrPhysMask3(_)
            | X86Register::MtrrPhysBase4(_)
            | X86Register::MtrrPhysMask4(_)
            | X86Register::MtrrFix64k00000(_)
            | X86Register::MtrrFix16k80000(_)
            | X86Register::MtrrFix4kE0000(_)
            | X86Register::MtrrFix4kE8000(_)
            | X86Register::MtrrFix4kF0000(_)
            | X86Register::MtrrFix4kF8000(_) => {
                tracing::warn!(?register, "Ignoring MTRR register for SNP.")
            }
        }
    }

    fn vp_context_page(&self) -> anyhow::Result<u64> {
        match self.acceptance {
            None => Err(anyhow::anyhow!("no vp context acceptance set")),
            Some(_) => Ok(self.page_number),
        }
    }

    fn set_vp_context_memory(&mut self, page_base: u64, acceptance: BootPageAcceptance) {
        assert!(self.acceptance.is_none(), "only allowed to set vmsa once");
        assert_eq!(
            acceptance,
            BootPageAcceptance::VpContext,
            "snp vp context memory must be VpContext"
        );

        self.page_number = page_base;
        self.acceptance = Some(acceptance);
    }

    fn finalize(mut self) -> Vec<VpContextState> {
        let mut state = Vec::new();

        let acceptance = match self.acceptance {
            None => return state,
            Some(acceptance) => acceptance,
        };

        assert_eq!(acceptance, BootPageAcceptance::VpContext);

        // If no paravisor is present, then generate a trampoline page to perform
        // validation of the low 1 MB of memory.  This is expected by UEFI and
        // normally performed by the HCL, but must be done in a trampoline if no
        // HCL is present.
        if self.accept_lower_1mb {
            let mut trampoline_page = vec![0u8; PAGE_SIZE_4K as usize];

            // Since this page is discarded immediately after it executes, it can
            // be placed anywhere in memory.  GPA page zero is a convenient unused
            // location.
            trampoline_page[..8].copy_from_slice(self.vmsa.rip.as_bytes());

            // Place a breakpoint at the front of the page to force a triple fault
            // in case of early failure.
            let break_offset = size_of::<u64>();
            trampoline_page[break_offset] = 0xCC;

            // Set RIP to the trampoline page.
            let mut byte_offset = break_offset + 1;
            self.vmsa.rip = byte_offset as u64;

            let copy_instr =
                |trampoline_page: &mut Vec<u8>, byte_offset, instruction: &[u8]| -> usize {
                    trampoline_page[byte_offset..byte_offset + instruction.len()]
                        .copy_from_slice(instruction);
                    byte_offset + instruction.len()
                };

            // mov esi, 01000h
            byte_offset = copy_instr(
                &mut trampoline_page,
                byte_offset,
                &[0xBE, 0x00, 0x10, 0x00, 0x00],
            );

            // mov ebx, 0100000h
            byte_offset = copy_instr(
                &mut trampoline_page,
                byte_offset,
                &[0xBB, 0x00, 0x00, 0x10, 0x00],
            );

            // xor ecx, ecx
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0x33, 0xC9]);

            // mov edx, 1
            byte_offset = copy_instr(
                &mut trampoline_page,
                byte_offset,
                &[0xBA, 0x01, 0x00, 0x00, 0x00],
            );

            // L1:
            let jump_offset = byte_offset;

            // mov eax, esi
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0x8B, 0xC6]);

            // pvalidate
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0xF2, 0x0F, 0x01, 0xFF]);

            // jc Break
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0x72]);
            byte_offset += 1;
            trampoline_page[byte_offset - 1] = (break_offset as u8).wrapping_sub(byte_offset as u8);

            // test rax, rax
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0x48, 0x85, 0xC0]);

            // jnz Break
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0x75]);
            byte_offset += 1;
            trampoline_page[byte_offset - 1] = (break_offset as u8).wrapping_sub(byte_offset as u8);

            // add esi, 01000h
            byte_offset = copy_instr(
                &mut trampoline_page,
                byte_offset,
                &[0x81, 0xC6, 0x00, 0x10, 0x00, 0x00],
            );

            // cmp esi, ebx
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0x3B, 0xF3]);

            // jb L1
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0x72]);
            byte_offset += 1;
            trampoline_page[byte_offset - 1] = (jump_offset as u8).wrapping_sub(byte_offset as u8);

            // jmp [0]
            byte_offset = copy_instr(&mut trampoline_page, byte_offset, &[0xFF, 0x25]);
            let relative_offset: u32 = 0u32.wrapping_sub(byte_offset as u32 + 4);
            trampoline_page[byte_offset..byte_offset + 4]
                .copy_from_slice(relative_offset.as_bytes());

            state.push(VpContextState::Page(VpContextPageState {
                page_base: 0,
                page_count: 1,
                acceptance: BootPageAcceptance::Exclusive,
                data: trampoline_page,
            }));
        }

        state.push(VpContextState::Page(VpContextPageState {
            page_base: self.page_number,
            page_count: 1,
            acceptance,
            data: self.vmsa.as_bytes().to_vec(),
        }));

        state
    }
}

/// Implementation of [`VpContextBuilder``] for a platform with AMD SEV-SNP
/// isolation.
#[derive(Debug)]
pub struct SnpVpContextBuilder {
    contexts: [SnpVpContext; HV_NUM_VTLS],
}

impl SnpVpContextBuilder {
    /// Create a new SNP VP context builder.
    ///
    /// `enlightened_uefi` specifies if UEFI is enlightened. This will result in
    /// [`VpContextBuilder::finalize`] generating additional trampoline code for
    /// UEFI running without a paravisor, along with setting different fields in
    /// the `SEV_FEATURES` register.
    ///
    /// `injection_type` specifies the injection type for the highest enabled
    /// VMPL.
    ///
    /// Only the highest VTL will have a VMSA generated, with lower VTLs being
    /// imported with the VBS format as page data.
    pub fn new(
        max_vtl: Vtl,
        enlightened_uefi: bool,
        shared_gpa_boundary: u64,
        injection_type: InjectionType,
    ) -> anyhow::Result<Self> {
        let mut contexts = [SnpVpContext::None, SnpVpContext::None, SnpVpContext::None];

        match max_vtl {
            Vtl::Vtl0 => {
                contexts[0] = SnpVpContext::Hardware(SnpHardwareContext::new(
                    Vtl::Vtl0,
                    enlightened_uefi,
                    shared_gpa_boundary,
                    injection_type,
                ))
            }
            Vtl::Vtl1 => anyhow::bail!("VTL1 import state not supported for SNP"),
            Vtl::Vtl2 => {
                // Treat VTL0 as the VBS format.
                contexts[0] = SnpVpContext::Vbs(VbsVpContext::new(0));
                contexts[2] = SnpVpContext::Hardware(SnpHardwareContext::new(
                    Vtl::Vtl2,
                    enlightened_uefi,
                    shared_gpa_boundary,
                    injection_type,
                ))
            }
        }

        Ok(Self { contexts })
    }
}

impl VpContextBuilder for SnpVpContextBuilder {
    type Register = X86Register;

    fn import_vp_register(&mut self, vtl: Vtl, register: X86Register) {
        self.contexts[vtl as usize].import_vp_register(register);
    }

    fn vp_context_page(&self, vtl: Vtl) -> anyhow::Result<u64> {
        self.contexts[vtl as usize].vp_context_page()
    }

    fn set_vp_context_memory(&mut self, vtl: Vtl, page_base: u64, acceptance: BootPageAcceptance) {
        self.contexts[vtl as usize].set_vp_context_memory(page_base, acceptance);
    }

    fn finalize(self: Box<Self>) -> Vec<VpContextState> {
        let mut state = Vec::new();

        for context in self.contexts {
            state.extend(context.finalize())
        }

        state
    }
}