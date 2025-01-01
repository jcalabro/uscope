//! This file contains various types, enums and constants that are defined by the DWARF debug info format

const std = @import("std");
const mem = std.mem;

const types = @import("../../types.zig");

pub const CompilationUnitHeaderType = enum(u8) {
    DW_UT_unknown = 0x00,
    DW_UT_compile = 0x01,
    DW_UT_type = 0x02,
    DW_UT_partial = 0x3,
    DW_UT_skeleton = 0x4,
    DW_UT_split_compile = 0x5,
    DW_UT_split_type = 0x6,
    DW_UT_lo_user = 0x7,
    DW_UT_hi_user = 0x8,
};

pub const AttributeTag = enum(u16) {
    DW_TAG_padding = 0x00,
    DW_TAG_array_type = 0x01,
    DW_TAG_class_type = 0x02,
    DW_TAG_entry_point = 0x03,
    DW_TAG_enumeration_type = 0x04,
    DW_TAG_formal_parameter = 0x05,
    DW_TAG_imported_declaration = 0x08,
    DW_TAG_label = 0x0a,
    DW_TAG_lexical_block = 0x0b,
    DW_TAG_member = 0x0d,
    DW_TAG_pointer_type = 0x0f,
    DW_TAG_reference_type = 0x10,
    DW_TAG_compile_unit = 0x11,
    DW_TAG_string_type = 0x12,
    DW_TAG_structure_type = 0x13,
    DW_TAG_subroutine = 0x14,
    DW_TAG_subroutine_type = 0x15,
    DW_TAG_typedef = 0x16,
    DW_TAG_union_type = 0x17,
    DW_TAG_unspecified_parameters = 0x18,
    DW_TAG_variant = 0x19,
    DW_TAG_common_block = 0x1a,
    DW_TAG_common_inclusion = 0x1b,
    DW_TAG_inheritance = 0x1c,
    DW_TAG_inlined_subroutine = 0x1d,
    DW_TAG_module = 0x1e,
    DW_TAG_ptr_to_member_type = 0x1f,
    DW_TAG_set_type = 0x20,
    DW_TAG_subrange_type = 0x21,
    DW_TAG_with_stmt = 0x22,
    DW_TAG_access_declaration = 0x23,
    DW_TAG_base_type = 0x24,
    DW_TAG_catch_block = 0x25,
    DW_TAG_const_type = 0x26,
    DW_TAG_constant = 0x27,
    DW_TAG_enumerator = 0x28,
    DW_TAG_file_type = 0x29,
    DW_TAG_friend = 0x2a,
    DW_TAG_namelist = 0x2b,
    DW_TAG_namelist_item = 0x2c,
    DW_TAG_packed_type = 0x2d,
    DW_TAG_subprogram = 0x2e,
    DW_TAG_template_type_param = 0x2f,
    DW_TAG_template_value_param = 0x30,
    DW_TAG_thrown_type = 0x31,
    DW_TAG_try_block = 0x32,
    DW_TAG_variant_part = 0x33,
    DW_TAG_variable = 0x34,
    DW_TAG_volatile_type = 0x35,

    // added in DWARF 3
    DW_TAG_dwarf_procedure = 0x36,
    DW_TAG_restrict_type = 0x37,
    DW_TAG_interface_type = 0x38,
    DW_TAG_namespace = 0x39,
    DW_TAG_imported_module = 0x3a,
    DW_TAG_unspecified_type = 0x3b,
    DW_TAG_partial_unit = 0x3c,
    DW_TAG_imported_unit = 0x3d,
    DW_TAG_condition = 0x3f,
    DW_TAG_shared_type = 0x40,

    // added in DWARF 4
    DW_TAG_type_unit = 0x41,
    DW_TAG_rvalue_reference_type = 0x42,
    DW_TAG_template_alias = 0x43,

    // added in DWARF 5
    DW_TAG_coarray_type = 0x44,
    DW_TAG_generic_subrange = 0x45,
    DW_TAG_dynamic_type = 0x46,
    DW_TAG_atomic_type = 0x47,
    DW_TAG_call_site = 0x48,
    DW_TAG_call_site_parameter = 0x49,
    DW_TAG_skeleton_unit = 0x4a,
    DW_TAG_immutable_type = 0x4b,

    DW_TAG_lo_user = 0x4080,
    DW_TAG_hi_user = 0xffff,

    // GNU extensions
    DW_TAG_format_label = 0x4101, // For FORTRAN 77 and Fortran 90.
    DW_TAG_function_template = 0x4102, // For C++.
    DW_TAG_class_template = 0x4103, // For C++.
    DW_TAG_GNU_BINCL = 0x4104,
    DW_TAG_GNU_EINCL = 0x4105,
    // http://gcc.gnu.org/wiki/TemplateParmsDwarf
    DW_TAG_GNU_template_template_param = 0x4106,
    DW_TAG_GNU_template_parameter_pack = 0x4107,
    DW_TAG_GNU_formal_parameter_pack = 0x4108,
    // http://www.dwarfstd.org/ShowIssue.php?issue=100909.2&type=open
    DW_TAG_GNU_call_site = 0x4109,
    DW_TAG_GNU_call_site_parameter = 0x410a,

    // Apple extensions
    DW_TAG_APPLE_property = 0x4200,

    // Zig extensions
    DW_TAG_ZIG_padding = 0xfdb1,
};

pub const AttributeName = enum(u32) {
    DW_AT_sibling = 0x01,
    DW_AT_location = 0x02,
    DW_AT_name = 0x03,
    DW_AT_ordering = 0x09,
    DW_AT_subscr_data = 0x0a,
    DW_AT_byte_size = 0x0b,
    DW_AT_bit_offset = 0x0c,
    DW_AT_bit_size = 0x0d,
    DW_AT_element_list = 0x0f,
    DW_AT_stmt_list = 0x10,
    DW_AT_low_pc = 0x11,
    DW_AT_high_pc = 0x12,
    DW_AT_language = 0x13,
    DW_AT_member = 0x14,
    DW_AT_discr = 0x15,
    DW_AT_discr_value = 0x16,
    DW_AT_visibility = 0x17,
    DW_AT_import = 0x18,
    DW_AT_string_length = 0x19,
    DW_AT_common_reference = 0x1a,
    DW_AT_comp_dir = 0x1b,
    DW_AT_const_value = 0x1c,
    DW_AT_containing_type = 0x1d,
    DW_AT_default_value = 0x1e,
    DW_AT_inline = 0x20,
    DW_AT_is_optional = 0x21,
    DW_AT_lower_bound = 0x22,
    DW_AT_producer = 0x25,
    DW_AT_prototyped = 0x27,
    DW_AT_return_addr = 0x2a,
    DW_AT_start_scope = 0x2c,
    DW_AT_bit_stride = 0x2e,
    DW_AT_upper_bound = 0x2f,
    DW_AT_abstract_origin = 0x31,
    DW_AT_accessibility = 0x32,
    DW_AT_address_class = 0x33,
    DW_AT_artificial = 0x34,
    DW_AT_base_types = 0x35,
    DW_AT_calling_convention = 0x36,
    DW_AT_count = 0x37,
    DW_AT_data_member_location = 0x38,
    DW_AT_decl_column = 0x39,
    DW_AT_decl_file = 0x3a,
    DW_AT_decl_line = 0x3b,
    DW_AT_declaration = 0x3c,
    DW_AT_discr_list = 0x3d,
    DW_AT_encoding = 0x3e,
    DW_AT_external = 0x3f,
    DW_AT_frame_base = 0x40,
    DW_AT_friend = 0x41,
    DW_AT_identifier_case = 0x42,
    DW_AT_macro_info = 0x43,
    DW_AT_namelist_items = 0x44,
    DW_AT_priority = 0x45,
    DW_AT_segment = 0x46,
    DW_AT_specification = 0x47,
    DW_AT_static_link = 0x48,
    DW_AT_type = 0x49,
    DW_AT_use_location = 0x4a,
    DW_AT_variable_parameter = 0x4b,
    DW_AT_virtuality = 0x4c,
    DW_AT_vtable_elem_location = 0x4d,

    // DWARF 3 values.
    DW_AT_allocated = 0x4e,
    DW_AT_associated = 0x4f,
    DW_AT_data_location = 0x50,
    DW_AT_byte_stride = 0x51,
    DW_AT_entry_pc = 0x52,
    DW_AT_use_UTF8 = 0x53,
    DW_AT_extension = 0x54,
    DW_AT_ranges = 0x55,
    DW_AT_trampoline = 0x56,
    DW_AT_call_column = 0x57,
    DW_AT_call_file = 0x58,
    DW_AT_call_line = 0x59,
    DW_AT_description = 0x5a,
    DW_AT_binary_scale = 0x5b,
    DW_AT_decimal_scale = 0x5c,
    DW_AT_small = 0x5d,
    DW_AT_decimal_sign = 0x5e,
    DW_AT_digit_count = 0x5f,
    DW_AT_picture_string = 0x60,
    DW_AT_mutable = 0x61,
    DW_AT_threads_scaled = 0x62,
    DW_AT_explicit = 0x63,
    DW_AT_object_pointer = 0x64,
    DW_AT_endianity = 0x65,
    DW_AT_elemental = 0x66,
    DW_AT_pure = 0x67,
    DW_AT_recursive = 0x68,

    // DWARF 4.
    DW_AT_signature = 0x69,
    DW_AT_main_subprogram = 0x6a,
    DW_AT_data_bit_offset = 0x6b,
    DW_AT_const_expr = 0x6c,
    DW_AT_enum_class = 0x6d,
    DW_AT_linkage_name = 0x6e,

    // DWARF 5
    DW_AT_string_length_bit_size = 0x6f,
    DW_AT_string_length_byte_size = 0x70,
    DW_AT_rank = 0x71,
    DW_AT_str_offsets_base = 0x72,
    DW_AT_addr_base = 0x73,
    DW_AT_rnglists_base = 0x74,
    DW_AT_dwo_name = 0x76,
    DW_AT_reference = 0x77,
    DW_AT_rvalue_reference = 0x78,
    DW_AT_macros = 0x79,
    DW_AT_call_all_calls = 0x7a,
    DW_AT_call_all_source_calls = 0x7b,
    DW_AT_call_all_tail_calls = 0x7c,
    DW_AT_call_return_pc = 0x7d,
    DW_AT_call_value = 0x7e,
    DW_AT_call_origin = 0x7f,
    DW_AT_call_parameter = 0x80,
    DW_AT_call_pc = 0x81,
    DW_AT_call_tail_call = 0x82,
    DW_AT_call_target = 0x83,
    DW_AT_call_target_clobbered = 0x84,
    DW_AT_call_data_location = 0x85,
    DW_AT_call_data_value = 0x86,
    DW_AT_noreturn = 0x87,
    DW_AT_alignment = 0x88,
    DW_AT_export_symbols = 0x89,
    DW_AT_deleted = 0x8a,
    DW_AT_defaulted = 0x8b,
    DW_AT_loclists_base = 0x8c,

    DW_AT_lo_user = 0x2000,
    DW_AT_hi_user = 0x3fff,

    // GNU extensions
    DW_AT_sf_names = 0x2101,
    DW_AT_src_info = 0x2102,
    DW_AT_mac_info = 0x2103,
    DW_AT_src_coords = 0x2104,
    DW_AT_body_begin = 0x2105,
    DW_AT_body_end = 0x2106,
    DW_AT_GNU_vector = 0x2107,
    DW_AT_GNU_guarded_by = 0x2108,
    DW_AT_GNU_pt_guarded_by = 0x2109,
    DW_AT_GNU_guarded = 0x210a,
    DW_AT_GNU_pt_guarded = 0x210b,
    DW_AT_GNU_locks_excluded = 0x210c,
    DW_AT_GNU_exclusive_locks_required = 0x210d,
    DW_AT_GNU_shared_locks_required = 0x210e,
    DW_AT_GNU_odr_signature = 0x210f,
    DW_AT_GNU_template_name = 0x2110,
    DW_AT_GNU_call_site_value = 0x2111,
    DW_AT_GNU_call_site_data_value = 0x2112,
    DW_AT_GNU_call_site_target = 0x2113,
    DW_AT_GNU_call_site_target_clobbered = 0x2114,
    DW_AT_GNU_tail_call = 0x2115,
    DW_AT_GNU_all_tail_call_sites = 0x2116,
    DW_AT_GNU_all_call_sites = 0x2117,
    DW_AT_GNU_all_source_call_sites = 0x2118,
    DW_AT_GNU_macros = 0x2119,
    DW_AT_GNU_deleted = 0x211a,

    // Extensions for Fission
    DW_AT_GNU_dwo_name = 0x2130,
    DW_AT_GNU_dwo_id = 0x2131,
    DW_AT_GNU_ranges_base = 0x2132,
    DW_AT_GNU_addr_base = 0x2133,
    DW_AT_GNU_pubnames = 0x2134,
    DW_AT_GNU_pubtypes = 0x2135,
    DW_AT_GNU_discriminator = 0x2136,
    DW_AT_GNU_locviews = 0x2137,
    DW_AT_GNU_entry_view = 0x2138,

    // LLVM extensions
    DW_AT_LLVM_include_path = 0x3e00,
    DW_AT_LLVM_config_macros = 0x3e01,
    DW_AT_LLVM_isysroot = 0x3e02,

    // Apple extensions
    DW_AT_APPLE_optimized = 0x3fe1,
    DW_AT_APPLE_flags = 0x3fe2,
    DW_AT_APPLE_isa = 0x3fe3,
    DW_AT_APPLE_block = 0x3fe4,
    DW_AT_APPLE_major_runtime_vers = 0x3fe5,
    DW_AT_APPLE_runtime_class = 0x3fe6,
    DW_AT_APPLE_omit_frame_ptr = 0x3fe7,
    DW_AT_APPLE_property_name = 0x3fe8,
    DW_AT_APPLE_property_getter = 0x3fe9,
    DW_AT_APPLE_property_setter = 0x3fea,
    DW_AT_APPLE_property_attribute = 0x3feb,
    DW_AT_APPLE_objc_complete_type = 0x3fec,
    DW_AT_APPLE_property = 0x3fed,

    // Golang extensions
    DW_AT_go_kind = 0x2900,
    DW_AT_go_key = 0x2901,
    DW_AT_go_elem = 0x2902,
    // Attribute for DW_TAG_member of a struct type.
    // Nonzero value indicates the struct field is an embedded field.
    DW_AT_go_embedded_field = 0x2903,
    DW_AT_go_runtime_type = 0x2904,

    DW_AT_go_package_name = 0x2905, // Attribute for DW_TAG_compile_unit
    DW_AT_go_dict_index = 0x2906, // Attribute for DW_TAG_typedef_type, index of the dictionary entry describing the real type of this type shape
    DW_AT_go_closure_offset = 0x2907, // Attribute for DW_TAG_variable, offset in the closure struct where this captured variable resides

    // DW_AT_internal_location = 253, // params and locals; not emitted
};

pub const AttributeForm = enum(u16) {
    DW_FORM_addr = 0x01,
    DW_FORM_block2 = 0x03,
    DW_FORM_block4 = 0x04,
    DW_FORM_data2 = 0x05,
    DW_FORM_data4 = 0x06,
    DW_FORM_data8 = 0x07,
    DW_FORM_string = 0x08,
    DW_FORM_block = 0x09,
    DW_FORM_block1 = 0x0a,
    DW_FORM_data1 = 0x0b,
    DW_FORM_flag = 0x0c,
    DW_FORM_sdata = 0x0d,
    DW_FORM_strp = 0x0e,
    DW_FORM_udata = 0x0f,
    DW_FORM_ref_addr = 0x10,
    DW_FORM_ref1 = 0x11,
    DW_FORM_ref2 = 0x12,
    DW_FORM_ref4 = 0x13,
    DW_FORM_ref8 = 0x14,
    DW_FORM_ref_udata = 0x15,
    DW_FORM_indirect = 0x16,
    DW_FORM_sec_offset = 0x17,
    DW_FORM_exprloc = 0x18,
    DW_FORM_flag_present = 0x19,
    DW_FORM_strx = 0x1a,
    DW_FORM_addrx = 0x1b,
    DW_FORM_ref_sup4 = 0x1c,
    DW_FORM_strp_sup = 0x1d,
    DW_FORM_data16 = 0x1e,
    DW_FORM_line_strp = 0x1f,
    DW_FORM_ref_sig8 = 0x20,
    DW_FORM_implicit_const = 0x21,
    DW_FORM_loclistx = 0x22,
    DW_FORM_rnglistx = 0x23,
    DW_FORM_ref_sup8 = 0x24,
    DW_FORM_strx1 = 0x25,
    DW_FORM_strx2 = 0x26,
    DW_FORM_strx3 = 0x27,
    DW_FORM_strx4 = 0x28,
    DW_FORM_addrx1 = 0x29,
    DW_FORM_addrx2 = 0x2a,
    DW_FORM_addrx3 = 0x2b,
    DW_FORM_addrx4 = 0x2c,

    // Extensions for Fission.  See http://gcc.gnu.org/wiki/DebugFission
    DW_FORM_GNU_addr_index = 0x1f01,
    DW_FORM_GNU_str_index = 0x1f02,

    // Extensions for DWZ multifile.
    // See http://www.dwarfstd.org/ShowIssue.php?issue=120604.1&type=open
    DW_FORM_GNU_ref_alt = 0x1f20,
    DW_FORM_GNU_strp_alt = 0x1f21,
};

pub const Language = enum(u16) {
    DW_LANG_C89 = 0x01,
    DW_LANG_C = 0x02,
    DW_LANG_Ada83 = 0x03,
    DW_LANG_C_plus_plus = 0x04,
    DW_LANG_Cobol74 = 0x05,
    DW_LANG_Cobol85 = 0x06,
    DW_LANG_Fortran77 = 0x07,
    DW_LANG_Fortran90 = 0x08,
    DW_LANG_Pascal83 = 0x09,
    DW_LANG_Modula2 = 0x0a,
    DW_LANG_Java = 0x0b,
    DW_LANG_C99 = 0x0c,
    DW_LANG_Ada95 = 0x0d,
    DW_LANG_Fortran95 = 0x0e,
    DW_LANG_PLI = 0x0f,
    DW_LANG_ObjC = 0x10,
    DW_LANG_ObjC_plus_plus = 0x11,
    DW_LANG_UPC = 0x12,
    DW_LANG_D = 0x13,
    DW_LANG_Python = 0x14,
    DW_LANG_OpenCL = 0x15,
    DW_LANG_Go = 0x16,
    DW_LANG_Modula3 = 0x17,
    DW_LANG_Haskell = 0x18,
    DW_LANG_C_plus_plus_03 = 0x19,
    DW_LANG_C_plus_plus_11 = 0x1a,
    DW_LANG_OCaml = 0x1b,
    DW_LANG_Rust = 0x1c,
    DW_LANG_C11 = 0x1d,
    DW_LANG_Swift = 0x1e,
    DW_LANG_Julia = 0x1f,
    DW_LANG_Dylan = 0x20,
    DW_LANG_C_plus_plus_14 = 0x21,
    DW_LANG_Fortran03 = 0x22,
    DW_LANG_Fortran08 = 0x23,
    DW_LANG_RenderScript = 0x24,
    DW_LANG_BLISS = 0x25,

    // added since DWARF v5, but not yet in the standard
    DW_LANG_Kotlin = 0x0026,
    DW_LANG_Zig = 0x0027,
    DW_LANG_Crystal = 0x0028,
    DW_LANG_C_plus_plus_17 = 0x002a,
    DW_LANG_C_plus_plus_20 = 0x002b,
    DW_LANG_C17 = 0x002c,
    DW_LANG_Fortran18 = 0x002d,
    DW_LANG_Ada2005 = 0x002e,
    DW_LANG_Ada2012 = 0x002f,
    DW_LANG_HIP = 0x0030,
    DW_LANG_Assembly = 0x0031,
    DW_LANG_C_sharp = 0x0032,
    DW_LANG_Mojo = 0x0033,

    DW_LANG_lo_user = 0x8000,
    DW_LANG_hi_user = 0xffff,

    /// https://dwarfstd.org/issues/210115.1.html
    DW_LANG_nasm = 0x8001,

    // not yet added, pick some random high value
    DW_LANG_Jai = 0xb103,
    DW_LANG_Odin = 0xb104,

    pub fn fromProducer(producer: []const u8) ?@This() {
        if (mem.startsWith(u8, producer, "zig")) return .DW_LANG_Zig;
        if (mem.startsWith(u8, producer, "odin")) return .DW_LANG_Odin;
        if (mem.containsAtLeast(u8, producer, 1, "Jai")) return .DW_LANG_Jai;

        return null;
    }

    pub fn toGeneric(self: @This()) error{LanguageUnsupported}!types.Language {
        return switch (self) {
            .DW_LANG_C,
            .DW_LANG_C89,
            .DW_LANG_C99,
            .DW_LANG_C11,
            .DW_LANG_C17,
            => return .C,

            .DW_LANG_C_plus_plus,
            .DW_LANG_C_plus_plus_03,
            .DW_LANG_C_plus_plus_11,
            .DW_LANG_C_plus_plus_14,
            .DW_LANG_C_plus_plus_17,
            .DW_LANG_C_plus_plus_20,
            => return .CPP,

            .DW_LANG_Go => return .Go,
            .DW_LANG_Rust => return .Rust,
            .DW_LANG_Zig => .Zig,
            .DW_LANG_Jai => .Jai,
            .DW_LANG_Odin => .Odin,

            .DW_LANG_nasm => .Assembly,

            else => error.LanguageUnsupported,
        };
    }

    pub fn str(self: @This()) []const u8 {
        return switch (self) {
            .DW_LANG_C,
            .DW_LANG_C89,
            .DW_LANG_C99,
            .DW_LANG_C11,
            .DW_LANG_C17,
            => "C",

            .DW_LANG_C_plus_plus,
            .DW_LANG_C_plus_plus_03,
            .DW_LANG_C_plus_plus_11,
            .DW_LANG_C_plus_plus_14,
            .DW_LANG_C_plus_plus_17,
            .DW_LANG_C_plus_plus_20,
            => "C++",

            .DW_LANG_Ada83 => "Ada 83",
            .DW_LANG_Cobol74 => "Cobol 74",
            .DW_LANG_Cobol85 => "Cobol 85",
            .DW_LANG_Fortran77 => "Fortran 77",
            .DW_LANG_Fortran90 => "Fortran 90",
            .DW_LANG_Pascal83 => "Pascal 83",
            .DW_LANG_Modula2 => "Modula-2",
            .DW_LANG_Java => "Java",
            .DW_LANG_Ada95 => "Ada 95",
            .DW_LANG_Fortran95 => "Fortran 95",
            .DW_LANG_PLI => "PLI",
            .DW_LANG_ObjC => "Objective-C",
            .DW_LANG_ObjC_plus_plus => "Objective-C++",
            .DW_LANG_UPC => "UPC",
            .DW_LANG_D => "D",
            .DW_LANG_Python => "Python",
            .DW_LANG_OpenCL => "OpenCL",
            .DW_LANG_Go => "Go",
            .DW_LANG_Modula3 => "Modula-3",
            .DW_LANG_Haskell => "Haskell",
            .DW_LANG_OCaml => "OCaml",
            .DW_LANG_Rust => "Rust",
            .DW_LANG_Swift => "Swift",
            .DW_LANG_Julia => "Julia",
            .DW_LANG_Dylan => "Dylan",
            .DW_LANG_Fortran03 => "Fortran 03",
            .DW_LANG_Fortran08 => "Fortran 08",
            .DW_LANG_RenderScript => "RenderScript",
            .DW_LANG_BLISS => "Bliss",

            .DW_LANG_Kotlin => "Kotlin",
            .DW_LANG_Zig => "Zig",
            .DW_LANG_Crystal => "Crystal",
            .DW_LANG_Fortran18 => "Fortran 18",
            .DW_LANG_Ada2005 => "Ada 2005",
            .DW_LANG_Ada2012 => "Ada 2012",
            .DW_LANG_HIP => "HIP",
            .DW_LANG_Assembly => "Assembly",
            .DW_LANG_C_sharp => "C#",
            .DW_LANG_Mojo => "Mojo",

            .DW_LANG_lo_user => "user-defined",
            .DW_LANG_hi_user => "user-defined",

            .DW_LANG_nasm => "Netwide Assembler",

            .DW_LANG_Jai => "Jai",
            .DW_LANG_Odin => "Odin",
        };
    }
};

pub const AttributeEncoding = enum(u8) {
    DW_ATE_address = 0x01,
    DW_ATE_boolean = 0x02,
    DW_ATE_complex_float = 0x03,
    DW_ATE_float = 0x04,
    DW_ATE_signed = 0x05,
    DW_ATE_signed_char = 0x06,
    DW_ATE_unsigned = 0x07,
    DW_ATE_unsigned_char = 0x08,

    // DWARF 3.
    DW_ATE_imaginary_float = 0x09,
    DW_ATE_packed_decimal = 0x0a,
    DW_ATE_numeric_string = 0x0b,
    DW_ATE_edited = 0x0c,
    DW_ATE_signed_fixed = 0x0d,
    DW_ATE_unsigned_fixed = 0x0e,
    DW_ATE_decimal_float = 0x0f,

    // DWARF 4.
    DW_ATE_UTF = 0x10,
    DW_ATE_UCS = 0x11,
    DW_ATE_ASCII = 0x12,

    // DW_ATE_lo_user = 0x80,
    // DW_ATE_hi_user = 0xff,
};

pub const RangeListEntry = enum(u8) {
    DW_RLE_end_of_list = 0x00,
    DW_RLE_base_addressx = 0x01,
    DW_RLE_startx_endx = 0x02,
    DW_RLE_startx_length = 0x03,
    DW_RLE_offset_pair = 0x04,
    DW_RLE_base_address = 0x05,
    DW_RLE_start_end = 0x06,
    DW_RLE_start_length = 0x07,
};

pub const LineNumberOpcodes = enum(u8) {
    copy = 1,
    advance_pc = 2,
    advance_line = 3,
    set_file = 4,
    set_column = 5,
    negate_stmt = 6,
    set_basic_block = 7,
    const_add_pc = 8,
    fixed_advance_pc = 9,

    // DWARF 3
    set_prologue_end = 10,
    set_epilogue_begin = 11,
    set_isa = 12,

    pub fn knownLen(self: @This()) ?u8 {
        return switch (self) {
            .copy => 0,
            .advance_pc => 1,
            .advance_line => 1,
            .set_file => 1,
            .negate_stmt => 0,
            .set_basic_block => 0,
            .const_add_pc => 0,
            .set_prologue_end => 0,
            .set_epilogue_begin => 0,
            .set_isa => 1,

            // .fixed_advance_pc takes a uint8 rather than a varint; it's
            // unclear what length the header is supposed to claim, so
            // ignore it.
            else => return null,
        };
    }
};

// Line table directory and file name entry formats.
// These are new in DWARF 5.
pub const LineTableContentType = enum(u8) {
    path = 0x01,
    directory_ndx = 0x02,
    timestamp = 0x03,
    size = 0x04,
    md5 = 0x05,
};

pub const LineTableStandardOpcodes = enum(u8) {
    copy = 0x01,
    advance_pc = 0x02,
    advance_line = 0x03,
    set_file = 0x04,
    set_column = 0x05,
    negate_stmt = 0x06,
    set_basic_block = 0x07,
    const_add_pc = 0x08,
    fixed_advance_pc = 0x09,
    set_prologue_end = 0x0a,
    set_epilogue_begin = 0x0b,
    set_isa = 0x0c,
};

pub const LineTableExtendedOpcodes = enum(u8) {
    end_sequence = 0x01,
    set_address = 0x02,
    define_file = 0x03,
    set_discriminator = 0x04,

    lo_user = 0x80,
    hi_user = 0xff,
};

pub const ExceptionHeaderFormat = enum(u8) {
    /// Unsigned value is encoded using the Little Endian Base 128 (LEB128) as defined by DWARF Debugging Information Format
    DW_EH_PE_uleb128 = 0x01,
    /// A 2 bytes unsigned value
    DW_EH_PE_udata2 = 0x02,
    /// A 4 bytes unsigned value
    DW_EH_PE_udata4 = 0x03,
    /// An 8 bytes unsigned value
    DW_EH_PE_udata8 = 0x04,
    // @NOTE (jrc) According to airs, DW_EH_PE_signed is not used in practice?
    // DW_EH_PE_signed = 0x08,
    /// Signed value is encoded using the Little Endian Base 128 (LEB128) as defined by DWARF Debugging Information Format
    DW_EH_PE_sleb128 = 0x09,
    /// A 2 bytes signed value
    DW_EH_PE_sdata2 = 0x0A,
    /// A 4 bytes signed value
    DW_EH_PE_sdata4 = 0x0B,
    /// An 8 bytes signed value
    DW_EH_PE_sdata8 = 0x0C,
    /// No value is present
    DW_EH_PE_omit = 0xff,
};

/// https://refspecs.linuxfoundation.org/LSB_2.1.0/LSB-Core-generic/LSB-Core-generic/dwarfehencoding.html
pub const ExceptionHeaderApplication = enum(u8) {
    /// Value is used with no modification
    DW_EH_PE_absptr = 0x00,
    /// Value is reletive to the current program counter
    DW_EH_PE_pcrel = 0x10,
    /// Value is reletive to the beginning of the .eh_frame_hdr section
    DW_EH_PE_datarel = 0x30,
    /// (Default): No value is present
    DW_EH_PE_omit = 0xff,
};

pub const CallFrameHighBitsInstruction = enum(u8) {
    /// Most opcodes do not have their high 2 bits set, so this flag will be active
    none = 0,

    DW_CFA_advance_loc = 0x01 << 6, // 64
    DW_CFA_offset = 0x02 << 6, // 128
    DW_CFA_restore = 0x03 << 6, // 192
};

pub const CallFrameInstruction = enum(u8) {
    DW_CFA_nop = 0,
    DW_CFA_set_loc = 0x01,
    DW_CFA_advance_loc1 = 0x02,
    DW_CFA_advance_loc2 = 0x03,
    DW_CFA_advance_loc4 = 0x04,
    DW_CFA_offset_extended = 0x05,
    DW_CFA_restore_extended = 0x06,
    DW_CFA_undefined = 0x07,
    DW_CFA_same_value = 0x08,
    DW_CFA_register = 0x09,
    DW_CFA_remember_state = 0x0a,
    DW_CFA_restore_state = 0x0b,
    DW_CFA_def_cfa = 0x0c,
    DW_CFA_def_cfa_register = 0x0d,
    DW_CFA_def_cfa_offset = 0x0e,
    DW_CFA_def_cfa_expression = 0x0f,
    DW_CFA_expression = 0x10,
    DW_CFA_offset_extended_sf = 0x11,
    DW_CFA_def_cfa_sf = 0x12,
    DW_CFA_def_cfa_offset_sf = 0x13,
    DW_CFA_val_offset = 0x14,
    DW_CFA_val_offset_sf = 0x15,
    DW_CFA_val_expression = 0x16,

    DW_CFA_lo_user = 0x1c,
    DW_CFA_hi_user = 0x3f,

    DW_CFA_MIPS_advance_loc8 = 0x1d,
    DW_CFA_GNU_window_save = 0x2d,
    DW_CFA_GNU_args_size = 0x2e,
    DW_CFA_GNU_negative_offset_extended = 0x2f,
};

/// AFAIK, these are not formally defined anywhere with "DW_" prefixes, but they
/// are inherently true about the register state
pub const UnwindRegisterRule = enum(u8) {
    /// Register value is not saved, it's lost
    undef,
    /// Register has same value as in prev. frame
    same,
    /// Register saved at CFA-relative address
    offset,
    /// Register is CFA-relative value
    val_offset,
    /// Register saved in a register
    reg,
    /// Register saved at computed value
    expr,
    /// Register is computed value
    val_expr,

    pub fn int(self: @This()) u8 {
        return @intFromEnum(self);
    }
};

pub const Registers = enum(u8) {
    rax,
    rdx,
    rcx,
    rbx,
    rsi,
    rdi,
    rbp,
    rsp,
    r8,
    r9,
    r10,
    r11,
    r12,
    r13,
    r14,
    r15,
    rip,

    pub fn int(self: @This()) u8 {
        return @intFromEnum(self);
    }
};

pub const ExpressionOpcode = enum(u8) {
    DW_OP_addr = 0x03,
    DW_OP_deref = 0x06,
    DW_OP_const1u = 0x08,
    DW_OP_const1s = 0x09,
    DW_OP_const2u = 0x0a,
    DW_OP_const2s = 0x0b,
    DW_OP_const4u = 0x0c,
    DW_OP_const4s = 0x0d,
    DW_OP_const8u = 0x0e,
    DW_OP_const8s = 0x0f,
    DW_OP_constu = 0x10,
    DW_OP_consts = 0x11,
    DW_OP_dup = 0x12,
    DW_OP_drop = 0x13,
    DW_OP_over = 0x14,
    DW_OP_pick = 0x15,
    DW_OP_swap = 0x16,
    DW_OP_rot = 0x17,
    DW_OP_xderef = 0x18,
    DW_OP_abs = 0x19,
    DW_OP_and = 0x1a,
    DW_OP_div = 0x1b,
    DW_OP_minus = 0x1c,
    DW_OP_mod = 0x1d,
    DW_OP_mul = 0x1e,
    DW_OP_neg = 0x1f,
    DW_OP_not = 0x20,
    DW_OP_or = 0x21,
    DW_OP_plus = 0x22,
    DW_OP_plus_uconst = 0x23,
    DW_OP_shl = 0x24,
    DW_OP_shr = 0x25,
    DW_OP_shra = 0x26,
    DW_OP_xor = 0x27,
    DW_OP_bra = 0x28,
    DW_OP_eq = 0x29,
    DW_OP_ge = 0x2a,
    DW_OP_gt = 0x2b,
    DW_OP_le = 0x2c,
    DW_OP_lt = 0x2d,
    DW_OP_ne = 0x2e,
    DW_OP_skip = 0x2f,
    DW_OP_lit0 = 0x30,
    DW_OP_lit1 = 0x31,
    DW_OP_lit2 = 0x32,
    DW_OP_lit3 = 0x33,
    DW_OP_lit4 = 0x34,
    DW_OP_lit5 = 0x35,
    DW_OP_lit6 = 0x36,
    DW_OP_lit7 = 0x37,
    DW_OP_lit8 = 0x38,
    DW_OP_lit9 = 0x39,
    DW_OP_lit10 = 0x3a,
    DW_OP_lit11 = 0x3b,
    DW_OP_lit12 = 0x3c,
    DW_OP_lit13 = 0x3d,
    DW_OP_lit14 = 0x3e,
    DW_OP_lit15 = 0x3f,
    DW_OP_lit16 = 0x40,
    DW_OP_lit17 = 0x41,
    DW_OP_lit18 = 0x42,
    DW_OP_lit19 = 0x43,
    DW_OP_lit20 = 0x44,
    DW_OP_lit21 = 0x45,
    DW_OP_lit22 = 0x46,
    DW_OP_lit23 = 0x47,
    DW_OP_lit24 = 0x48,
    DW_OP_lit25 = 0x49,
    DW_OP_lit26 = 0x4a,
    DW_OP_lit27 = 0x4b,
    DW_OP_lit28 = 0x4c,
    DW_OP_lit29 = 0x4d,
    DW_OP_lit30 = 0x4e,
    DW_OP_lit31 = 0x4f,
    DW_OP_reg0 = 0x50,
    DW_OP_reg1 = 0x51,
    DW_OP_reg2 = 0x52,
    DW_OP_reg3 = 0x53,
    DW_OP_reg4 = 0x54,
    DW_OP_reg5 = 0x55,
    DW_OP_reg6 = 0x56,
    DW_OP_reg7 = 0x57,
    DW_OP_reg8 = 0x58,
    DW_OP_reg9 = 0x59,
    DW_OP_reg10 = 0x5a,
    DW_OP_reg11 = 0x5b,
    DW_OP_reg12 = 0x5c,
    DW_OP_reg13 = 0x5d,
    DW_OP_reg14 = 0x5e,
    DW_OP_reg15 = 0x5f,
    DW_OP_reg16 = 0x60,
    DW_OP_reg17 = 0x61,
    DW_OP_reg18 = 0x62,
    DW_OP_reg19 = 0x63,
    DW_OP_reg20 = 0x64,
    DW_OP_reg21 = 0x65,
    DW_OP_reg22 = 0x66,
    DW_OP_reg23 = 0x67,
    DW_OP_reg24 = 0x68,
    DW_OP_reg25 = 0x69,
    DW_OP_reg26 = 0x6a,
    DW_OP_reg27 = 0x6b,
    DW_OP_reg28 = 0x6c,
    DW_OP_reg29 = 0x6d,
    DW_OP_reg30 = 0x6e,
    DW_OP_reg31 = 0x6f,
    DW_OP_breg0 = 0x70,
    DW_OP_breg1 = 0x71,
    DW_OP_breg2 = 0x72,
    DW_OP_breg3 = 0x73,
    DW_OP_breg4 = 0x74,
    DW_OP_breg5 = 0x75,
    DW_OP_breg6 = 0x76,
    DW_OP_breg7 = 0x77,
    DW_OP_breg8 = 0x78,
    DW_OP_breg9 = 0x79,
    DW_OP_breg10 = 0x7a,
    DW_OP_breg11 = 0x7b,
    DW_OP_breg12 = 0x7c,
    DW_OP_breg13 = 0x7d,
    DW_OP_breg14 = 0x7e,
    DW_OP_breg15 = 0x7f,
    DW_OP_breg16 = 0x80,
    DW_OP_breg17 = 0x81,
    DW_OP_breg18 = 0x82,
    DW_OP_breg19 = 0x83,
    DW_OP_breg20 = 0x84,
    DW_OP_breg21 = 0x85,
    DW_OP_breg22 = 0x86,
    DW_OP_breg23 = 0x87,
    DW_OP_breg24 = 0x88,
    DW_OP_breg25 = 0x89,
    DW_OP_breg26 = 0x8a,
    DW_OP_breg27 = 0x8b,
    DW_OP_breg28 = 0x8c,
    DW_OP_breg29 = 0x8d,
    DW_OP_breg30 = 0x8e,
    DW_OP_breg31 = 0x8f,
    DW_OP_regx = 0x90,
    DW_OP_fbreg = 0x91,
    DW_OP_bregx = 0x92,
    DW_OP_piece = 0x93,
    DW_OP_deref_size = 0x94,
    DW_OP_xderef_size = 0x95,
    DW_OP_nop = 0x96,

    // DWARF 3 extensions.
    DW_OP_push_object_address = 0x97,
    DW_OP_call2 = 0x98,
    DW_OP_call4 = 0x99,
    DW_OP_call_ref = 0x9a,
    DW_OP_form_tls_address = 0x9b,
    DW_OP_call_frame_cfa = 0x9c,
    DW_OP_bit_piece = 0x9d,

    // DWARF 4 extensions.
    DW_OP_implicit_value = 0x9e,
    DW_OP_stack_value = 0x9f,

    // DWARF 5 extensions.
    DW_OP_implicit_pointer = 0xa0,
    DW_OP_addrx = 0xa1,
    DW_OP_constx = 0xa2,
    DW_OP_entry_value = 0xa3,
    DW_OP_const_type = 0xa4,
    DW_OP_regval_type = 0xa5,
    DW_OP_deref_type = 0xa6,
    DW_OP_xderef_type = 0xa7,
    DW_OP_convert = 0xa8,
    DW_OP_reinterpret = 0xa9,

    // GNU extensions.
    DW_OP_GNU_push_tls_address = 0xe0,
    // The following is for marking variables that are uninitialized.
    DW_OP_GNU_uninit = 0xf0,
    DW_OP_GNU_encoded_addr = 0xf1,
    // The GNU implicit pointer extension.
    // See http://www.dwarfstd.org/ShowIssue.php?issue=100831.1&type=open .
    DW_OP_GNU_implicit_pointer = 0xf2,
    // The GNU entry value extension.
    // See http://www.dwarfstd.org/ShowIssue.php?issue=100909.1&type=open .
    DW_OP_GNU_entry_value = 0xf3,
    // The GNU typed stack extension.
    // See http://www.dwarfstd.org/doc/040408.1.html .
    DW_OP_GNU_const_type = 0xf4,
    DW_OP_GNU_regval_type = 0xf5,
    DW_OP_GNU_deref_type = 0xf6,
    DW_OP_GNU_convert = 0xf7,
    DW_OP_GNU_reinterpret = 0xf9,
    // The GNU parameter ref extension.
    DW_OP_GNU_parameter_ref = 0xfa,
    // Extension for Fission.  See http://gcc.gnu.org/wiki/DebugFission.
    // DW_OP_GNU_addr_index = 0xfb,
    // DW_OP_GNU_const_index = 0xfc,
    // HP extensions.
    // DW_OP_HP_unknown = 0xe0, // Duplicate of GNU_push_tls_address,
    // DW_OP_HP_is_value = 0xe1,
    // DW_OP_HP_fltconst4 = 0xe2,
    // DW_OP_HP_fltconst8 = 0xe3,
    // DW_OP_HP_mod_range = 0xe4,
    // DW_OP_HP_unmod_range = 0xe5,
    // DW_OP_HP_tls = 0xe6,
    // PGI (STMicroelectronics) extensions.
    // DW_OP_PGI_omp_thread_num = 0xf8,
    // Wasm extensions.
    // DW_OP_WASM_location = 0xed,
    // DW_OP_WASM_local = 0x00,
    // DW_OP_WASM_global = 0x01,
    // DW_OP_WASM_global_u32 = 0x03,
    // DW_OP_WASM_operand_stack = 0x02,

    pub const lo_user = 0xe0; // Implementation-defined range start.
    pub const hi_user = 0xff; // Implementation-defined range end.

    pub fn int(self: @This()) u8 {
        return @intFromEnum(self);
    }
};
