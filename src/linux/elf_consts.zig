//! This file contains various types, enums, and constants that are defined by the ELF file format

pub const MAGIC = "\x7fELF";

// sizes of various types
pub const HALF = @sizeOf(u16);
pub const WORD = @sizeOf(u32);
pub const SWORD = @sizeOf(i32);
pub const XWORD = @sizeOf(u64);
pub const SXWORD = @sizeOf(i64);
pub const ADDR = @sizeOf(usize);
pub const OFF = @sizeOf(usize);
pub const SECTION = @sizeOf(u16);

pub const EI_CLASS = 4; // machine class (32 or 64 bit architecture)
pub const EI_DATA = 5; // data format (byte order)
pub const EI_VERSION = 6; // ELF format version
pub const EI_OSABI = 7; // OS/ABI identification
pub const EI_ABIVERSION = 8; // ABI version
pub const EI_PAD = 9; // Start of padding
pub const EI_NIDENT = 16; // the number of bytes of identifier data

pub const Class = enum(u2) {
    none = 0,
    @"32" = 1, // 32 bit architecture
    @"64" = 2, // 64 bit architecture
};

pub const Data = enum(u2) {
    unknown = 0,
    @"2lsb" = 1, // 2's compliment little-endian
    @"2msb" = 2, // 2's compliment big-endian
};

pub const Version = enum(u2) {
    unknown = 0,
    current = 1,
};

pub const OSABI = enum(u8) {
    sysv = 0, // shares the zero value with "none"
    hpux = 1,
    netbsd = 2,
    linux = 3,
    hurd = 4,
    open_86 = 5,
    solaris = 6,
    aix = 7,
    irix = 8,
    freebsd = 9,
    tru64 = 10,
    modesto = 11,
    openbsd = 12,
    openvms = 13,
    nsk = 14,
    aros = 15,
    fenixos = 16,
    cloudabi = 17,
    arm = 97,
    standalone = 255,
};

pub const FileType = enum(u3) {
    none = 0,
    relocatable = 1,
    executable = 2,
    dynamic = 3, // shared object
    core = 4,

    // types between loproc and hiproc are processor-specific (not supported at this time)
    // loproc = 0xff00,
    // hiproc = 0xffff,
};

// @SRC: https://pkg.go.dev/debug/elf#SectionType
pub const SectionType = enum(u32) {
    null = 0, // inactive
    progbits = 1, // program defined information
    symtab = 2, // symbol table section
    strtab = 3, // string table section
    rela = 4, // relocation section with addends
    hash = 5, // symbol hash table section
    dynamic = 6, // dynamic section
    note = 7, // note section
    nobits = 8, // no space section
    rel = 9, // relocation section - no addends
    shlib = 10, // reserved - purpose unknown
    dynsym = 11, // dynamic symbol table section
    init_array = 14, // Initialization function pointers.
    fini_array = 15, // Termination function pointers.
    preinit_array = 16, // Pre-initialization function ptrs.
    group = 17, // Section group.
    symtab_shndx = 18, // Section indexes (see SHN_XINDEX).
    loos = 0x60000000, // First of OS specific semantics
    gnu_attributes = 0x6ffffff5, // GNU object attributes
    gnu_hash = 0x6ffffff6, // GNU hash table
    gnu_liblist = 0x6ffffff7, // GNU prelink library list
    gnu_verdef = 0x6ffffffd, // GNU version definition section
    gnu_verneed = 0x6ffffffe, // GNU version needs section
    gnu_versym = 0x6fffffff, // GNU version symbol table
    // hios = 0x6fffffff, // Last of OS specific semantics
    loproc = 0x70000000, // reserved range for processor
    mips_abiflags = 0x7000002a, // .MIPS.abiflags
    hiproc = 0x7fffffff, // specific section header types
    louser = 0x80000000, // reserved range for application
    hiuser = 0xffffffff, // specific indexes
};

// @SRC: https://github.com/ziglang/zig/blob/9ccd8ed0ad4cc9e68c2a2e0c9b1e32d50259357e/lib/std/elf.zig#L950
pub const Machine = enum(u16) {
    // no machine
    none = 0,

    // at&t we 32100
    m32 = 1,

    // sparc
    sparc = 2,

    // intel 386
    @"386" = 3,

    // motorola 68000
    @"68k" = 4,

    // motorola 88000
    @"88k" = 5,

    // intel mcu
    iamcu = 6,

    // intel 80860
    @"860" = 7,

    // mips r3000
    mips = 8,

    // ibm system/370
    s370 = 9,

    // mips rs3000 little-endian
    mips_rs3_le = 10,

    // spu mark ii
    spu_2 = 13,

    // hewlett-packard pa-risc
    parisc = 15,

    // fujitsu vpp500
    vpp500 = 17,

    // enhanced instruction set sparc
    sparc32plus = 18,

    // intel 80960
    @"960" = 19,

    // powerpc
    ppc = 20,

    // powerpc64
    ppc64 = 21,

    // ibm system/390
    s390 = 22,

    // ibm spu/spc
    spu = 23,

    // nec v800
    v800 = 36,

    // fujitsu fr20
    fr20 = 37,

    // trw rh-32
    rh32 = 38,

    // motorola rce
    rce = 39,

    // arm
    arm = 40,

    // dec alpha
    alpha = 41,

    // hitachi sh
    sh = 42,

    // sparc v9
    sparcv9 = 43,

    // siemens tricore
    tricore = 44,

    // argonaut risc core
    arc = 45,

    // hitachi h8/300
    h8_300 = 46,

    // hitachi h8/300h
    h8_300h = 47,

    // hitachi h8s
    h8s = 48,

    // hitachi h8/500
    h8_500 = 49,

    // intel ia-64 processor architecture
    ia_64 = 50,

    // stanford mips-x
    mips_x = 51,

    // motorola coldfire
    coldfire = 52,

    // motorola m68hc12
    @"68hc12" = 53,

    // fujitsu mma multimedia accelerator
    mma = 54,

    // siemens pcp
    pcp = 55,

    // sony ncpu embedded risc processor
    ncpu = 56,

    // denso ndr1 microprocessor
    ndr1 = 57,

    // motorola star*core processor
    starcore = 58,

    // toyota me16 processor
    me16 = 59,

    // stmicroelectronics st100 processor
    st100 = 60,

    // advanced logic corp. tinyj embedded processor family
    tinyj = 61,

    // amd x86-64 architecture
    x86_64 = 62,

    // sony dsp processor
    pdsp = 63,

    // digital equipment corp. pdp-10
    pdp10 = 64,

    // digital equipment corp. pdp-11
    pdp11 = 65,

    // siemens fx66 microcontroller
    fx66 = 66,

    // stmicroelectronics st9+ 8/16 bit microcontroller
    st9plus = 67,

    // stmicroelectronics st7 8-bit microcontroller
    st7 = 68,

    // motorola mc68hc16 microcontroller
    @"68hc16" = 69,

    // motorola mc68hc11 microcontroller
    @"68hc11" = 70,

    // motorola mc68hc08 microcontroller
    @"68hc08" = 71,

    // motorola mc68hc05 microcontroller
    @"68hc05" = 72,

    // silicon graphics svx
    svx = 73,

    // stmicroelectronics st19 8-bit microcontroller
    st19 = 74,

    // digital vax
    vax = 75,

    // axis communications 32-bit embedded processor
    cris = 76,

    // infineon technologies 32-bit embedded processor
    javelin = 77,

    // element 14 64-bit dsp processor
    firepath = 78,

    // lsi logic 16-bit dsp processor
    zsp = 79,

    // donald knuth's educational 64-bit processor
    mmix = 80,

    // harvard university machine-independent object files
    huany = 81,

    // sitera prism
    prism = 82,

    // atmel avr 8-bit microcontroller
    avr = 83,

    // fujitsu fr30
    fr30 = 84,

    // mitsubishi d10v
    d10v = 85,

    // mitsubishi d30v
    d30v = 86,

    // nec v850
    v850 = 87,

    // mitsubishi m32r
    m32r = 88,

    // matsushita mn10300
    mn10300 = 89,

    // matsushita mn10200
    mn10200 = 90,

    // picojava
    pj = 91,

    // openrisc 32-bit embedded processor
    openrisc = 92,

    // arc international arcompact processor (old spelling/synonym: em_arc_a5)
    arc_compact = 93,

    // tensilica xtensa architecture
    xtensa = 94,

    // alphamosaic videocore processor
    videocore = 95,

    // thompson multimedia general purpose processor
    tmm_gpp = 96,

    // national semiconductor 32000 series
    ns32k = 97,

    // tenor network tpc processor
    tpc = 98,

    // trebia snp 1000 processor
    snp1k = 99,

    // stmicroelectronics (www.st.com) st200
    st200 = 100,

    // ubicom ip2xxx microcontroller family
    ip2k = 101,

    // max processor
    max = 102,

    // national semiconductor compactrisc microprocessor
    cr = 103,

    // fujitsu f2mc16
    f2mc16 = 104,

    // texas instruments embedded microcontroller msp430
    msp430 = 105,

    // analog devices blackfin (dsp) processor
    blackfin = 106,

    // s1c33 family of seiko epson processors
    se_c33 = 107,

    // sharp embedded microprocessor
    sep = 108,

    // arca risc microprocessor
    arca = 109,

    // microprocessor series from pku-unity ltd. and mprc of peking university
    unicore = 110,

    // excess: 16/32/64-bit configurable embedded cpu
    excess = 111,

    // icera semiconductor inc. deep execution processor
    dxp = 112,

    // altera nios ii soft-core processor
    altera_nios2 = 113,

    // national semiconductor compactrisc crx
    crx = 114,

    // motorola xgate embedded processor
    xgate = 115,

    // infineon c16x/xc16x processor
    c166 = 116,

    // renesas m16c series microprocessors
    m16c = 117,

    // microchip technology dspic30f digital signal controller
    dspic30f = 118,

    // freescale communication engine risc core
    ce = 119,

    // renesas m32c series microprocessors
    m32c = 120,

    // altium tsk3000 core
    tsk3000 = 131,

    // freescale rs08 embedded processor
    rs08 = 132,

    // analog devices sharc family of 32-bit dsp processors
    sharc = 133,

    // cyan technology ecog2 microprocessor
    ecog2 = 134,

    // sunplus s+core7 risc processor
    score7 = 135,

    // new japan radio (njr) 24-bit dsp processor
    dsp24 = 136,

    // broadcom videocore iii processor
    videocore3 = 137,

    // risc processor for lattice fpga architecture
    latticemico32 = 138,

    // seiko epson c17 family
    se_c17 = 139,

    // the texas instruments tms320c6000 dsp family
    ti_c6000 = 140,

    // the texas instruments tms320c2000 dsp family
    ti_c2000 = 141,

    // the texas instruments tms320c55x dsp family
    ti_c5500 = 142,

    // stmicroelectronics 64bit vliw data signal processor
    mmdsp_plus = 160,

    // cypress m8c microprocessor
    cypress_m8c = 161,

    // renesas r32c series microprocessors
    r32c = 162,

    // nxp semiconductors trimedia architecture family
    trimedia = 163,

    // qualcomm hexagon processor
    hexagon = 164,

    // intel 8051 and variants
    @"8051" = 165,

    // stmicroelectronics stxp7x family of configurable and extensible risc processors
    stxp7x = 166,

    // andes technology compact code size embedded risc processor family
    nds32 = 167,

    // cyan technology ecog1x family
    ecog1x = 168,

    // dallas semiconductor maxq30 core micro-controllers
    maxq30 = 169,

    // new japan radio (njr) 16-bit dsp processor
    ximo16 = 170,

    // m2000 reconfigurable risc microprocessor
    manik = 171,

    // cray inc. nv2 vector architecture
    craynv2 = 172,

    // renesas rx family
    rx = 173,

    // imagination technologies meta processor architecture
    metag = 174,

    // mcst elbrus general purpose hardware architecture
    mcst_elbrus = 175,

    // cyan technology ecog16 family
    ecog16 = 176,

    // national semiconductor compactrisc cr16 16-bit microprocessor
    cr16 = 177,

    // freescale extended time processing unit
    etpu = 178,

    // infineon technologies sle9x core
    sle9x = 179,

    // intel l10m
    l10m = 180,

    // intel k10m
    k10m = 181,

    // arm aarch64
    aarch64 = 183,

    // atmel corporation 32-bit microprocessor family
    avr32 = 185,

    // stmicroeletronics stm8 8-bit microcontroller
    stm8 = 186,

    // tilera tile64 multicore architecture family
    tile64 = 187,

    // tilera tilepro multicore architecture family
    tilepro = 188,

    // nvidia cuda architecture
    cuda = 190,

    // tilera tile-gx multicore architecture family
    tilegx = 191,

    // cloudshield architecture family
    cloudshield = 192,

    // kipo-kaist core-a 1st generation processor family
    corea_1st = 193,

    // kipo-kaist core-a 2nd generation processor family
    corea_2nd = 194,

    // synopsys arcompact v2
    arc_compact2 = 195,

    // open8 8-bit risc soft processor core
    open8 = 196,

    // renesas rl78 family
    rl78 = 197,

    // broadcom videocore v processor
    videocore5 = 198,

    // renesas 78kor family
    @"78kor" = 199,

    // freescale 56800ex digital signal controller (dsc)
    @"56800ex" = 200,

    // beyond ba1 cpu architecture
    ba1 = 201,

    // beyond ba2 cpu architecture
    ba2 = 202,

    // xmos xcore processor family
    xcore = 203,

    // microchip 8-bit pic(r) family
    mchp_pic = 204,

    // reserved by intel
    intel205 = 205,

    // reserved by intel
    intel206 = 206,

    // reserved by intel
    intel207 = 207,

    // reserved by intel
    intel208 = 208,

    // reserved by intel
    intel209 = 209,

    // km211 km32 32-bit processor
    km32 = 210,

    // km211 kmx32 32-bit processor
    kmx32 = 211,

    // km211 kmx16 16-bit processor
    kmx16 = 212,

    // km211 kmx8 8-bit processor
    kmx8 = 213,

    // km211 kvarc processor
    kvarc = 214,

    // paneve cdp architecture family
    cdp = 215,

    // cognitive smart memory processor
    coge = 216,

    // icelero coolengine
    cool = 217,

    // nanoradio optimized risc
    norc = 218,

    // csr kalimba architecture family
    csr_kalimba = 219,

    // amd gpu architecture
    amdgpu = 224,

    // risc-v
    riscv = 243,

    // lanai 32-bit processor
    lanai = 244,

    // linux kernel bpf virtual machine
    bpf = 247,

    // c-sky
    csky = 252,

    // fujitsu fr-v
    frv = 0x5441,
};
