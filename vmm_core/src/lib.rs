// Copyright (C) Microsoft Corporation. All rights reserved.

//! Core building blocks for managing vm and vm related state to build a vmm.
//! Used by both hvlite and underhill today.

pub mod acpi_builder;
pub mod cpuid;
pub mod device_builder;
pub mod emuplat;
pub mod input_distributor;
pub mod partition_unit;
pub mod platform_resolvers;
pub mod synic;
pub mod vmbus_unit;
pub mod vmotherboard_adapter;
pub mod vmtime_unit;