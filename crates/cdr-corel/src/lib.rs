#[cfg(windows)]
mod windows_adapter {
    use base64::Engine;
    use cdr_domain::{ClosedPath, PathSegment as DomainPathSegment, PointMm as DomainPointMm};
    use cdr_raster::{
        AlphaMask, HolePlacementSettings, OutlineSettings, add_top_center_hole_ring,
        combine_top_center_hole,
    };
    use geo::{LineString, MultiPolygon};
    use image::{ImageReader, RgbaImage, imageops};
    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;
    use std::error::Error;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::{io, ptr};
    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, CLSIDFromProgID, COINIT_APARTMENTTHREADED, CoCreateInstance,
        CoInitializeEx, CoUninitialize, DISPATCH_FLAGS, DISPATCH_METHOD, DISPATCH_PROPERTYGET,
        DISPATCH_PROPERTYPUT, DISPPARAMS, IDispatch, IDispatch_Vtbl,
    };
    use windows::Win32::System::Ole::DISPID_PROPERTYPUT;
    use windows::Win32::System::Variant::{VT_DISPATCH, VT_UNKNOWN};
    use windows::core::{BSTR, GUID, IUnknown, Interface, PCWSTR, VARIANT};

    const CORELDRAW_PROG_ID: &str = "CorelDRAW.Application.22";
    const CORELDRAW_LAYER_IMPORT_SLOT: usize = 28;
    const MILLIMETER_UNIT: i32 = 3;
    const OUTLINE_OFFSET_MM: f64 = 2.0;
    const HOLE_DIAMETER_MM: f64 = 3.5;
    const HOLE_EDGE_CLEARANCE_MM: f64 = 2.0;
    const TOOL_DIAMETER_MM: f64 = 2.0;
    const SMOOTHING_MM: f64 = 0.02;
    const ALPHA_THRESHOLD: u8 = 127;
    const MINIMUM_COMPONENT_AREA_MM2: f64 = 0.25;
    const TRIM_ALPHA_THRESHOLD: u8 = 96;

    type Result<T> = std::result::Result<T, Box<dyn Error>>;

    #[derive(Debug, Clone, Copy)]
    pub struct ProcessingSettings {
        pub outline_offset_mm: f64,
        pub hole_diameter_mm: f64,
        pub hole_edge_clearance_mm: f64,
        pub tool_diameter_mm: f64,
        pub smoothing_mm: f64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SelectionOperation {
        Outline,
        AddHoles,
        MergeHoles,
        TrimTransparent,
    }

    impl SelectionOperation {
        fn layer_name(self) -> &'static str {
            match self {
                Self::Outline => "RUST_OUTLINE",
                Self::AddHoles => "RUST_HOLES",
                Self::MergeHoles => "RUST_MERGED",
                Self::TrimTransparent => "RUST_TRIMMED_OUTLINE",
            }
        }

        fn progress_prefix(self) -> &'static str {
            match self {
                Self::Outline => "巡边",
                Self::AddHoles => "加孔",
                Self::MergeHoles => "合并孔位",
                Self::TrimTransparent => "去透明边并缩放",
            }
        }
    }

    impl Default for ProcessingSettings {
        fn default() -> Self {
            Self {
                outline_offset_mm: OUTLINE_OFFSET_MM,
                hole_diameter_mm: HOLE_DIAMETER_MM,
                hole_edge_clearance_mm: HOLE_EDGE_CLEARANCE_MM,
                tool_diameter_mm: TOOL_DIAMETER_MM,
                smoothing_mm: SMOOTHING_MM,
            }
        }
    }

    impl ProcessingSettings {
        fn validate(self) -> Result<()> {
            if !self.outline_offset_mm.is_finite()
                || self.outline_offset_mm <= 0.0
                || !self.hole_diameter_mm.is_finite()
                || self.hole_diameter_mm < 0.0
                || !self.hole_edge_clearance_mm.is_finite()
                || self.hole_edge_clearance_mm <= 0.0
                || !self.tool_diameter_mm.is_finite()
                || self.tool_diameter_mm < 0.0
                || !self.smoothing_mm.is_finite()
                || self.smoothing_mm < 0.0
            {
                return Err(io::Error::other(
                    "巡边和孔边距必须大于 0；孔径和平滑值必须大于或等于 0",
                )
                .into());
            }
            Ok(())
        }
    }

    #[derive(Debug, Clone)]
    pub struct ProcessOutcome {
        pub document_name: String,
        pub selected_count: i32,
        pub processed_count: usize,
        pub untouched_hole_count: usize,
        pub skipped_no_alpha: Vec<String>,
        pub output_layer: String,
        pub operation: SelectionOperation,
    }

    struct MergeSelectionShape {
        shape: Dispatch,
        curve: Dispatch,
        parent_group: Option<Dispatch>,
        is_hole: bool,
        inner_rings: Vec<ClosedPath>,
    }

    struct BitmapReplacement {
        source: Dispatch,
        source_static_id: i32,
        png_path: std::path::PathBuf,
        bounds: BoundsMm,
    }

    struct SvgBitmap {
        shape: Dispatch,
        original_name: String,
        requires_replacement: bool,
    }

    struct SvgImageTag {
        range: std::ops::Range<usize>,
        id: Option<String>,
        geometry: [Option<String>; 6],
        parent_matrix: [f64; 6],
        embedded_data: bool,
    }

    const fn default_page_index() -> i32 {
        1
    }

    #[derive(Debug, Deserialize)]
    struct VectorOutput {
        schema_version: u32,
        source_sha256: String,
        page_index: i32,
        shapes: Vec<VectorShape>,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct VectorShape {
        source_shape_path: String,
        name: String,
        outline_hex: String,
        outline_width_mm: f64,
        polygons: Vec<VectorPolygon>,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct VectorPolygon {
        exterior: Vec<PointMm>,
        interiors: Vec<Vec<PointMm>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        curved_exterior: Option<ClosedPath>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        curved_interiors: Option<Vec<ClosedPath>>,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct PointMm {
        x: f64,
        y: f64,
    }

    #[derive(Debug, Serialize)]
    struct VerificationReport {
        schema_version: u32,
        coreldraw_version: String,
        source_sha256: String,
        output_sha256: String,
        original_unchanged: bool,
        layer_name: &'static str,
        shape_count: i32,
        shapes: Vec<VerifiedShape>,
    }

    #[derive(Debug, Serialize)]
    struct VerifiedShape {
        source_shape_path: String,
        name: String,
        shape_type: i32,
        subpath_count: i32,
        closed_subpaths: i32,
        fill_type: i32,
        outline_type: i32,
        outline_width_mm: f64,
        outline_rgb: [i32; 3],
        bounds_mm: BoundsMm,
    }

    #[derive(Debug, Clone, Copy, Serialize)]
    struct BoundsMm {
        left: f64,
        bottom: f64,
        width: f64,
        height: f64,
    }

    #[derive(Debug, Clone, Copy)]
    struct DocumentUnits {
        millimeters_per_unit: f64,
    }

    impl DocumentUnits {
        fn to_mm(self, value: f64) -> f64 {
            value * self.millimeters_per_unit
        }

        fn value_from_mm(self, value: f64) -> f64 {
            value / self.millimeters_per_unit
        }
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct SelectionReport {
        schema_version: u32,
        coreldraw_version: String,
        document_name: String,
        #[serde(default = "default_page_index")]
        page_index: i32,
        selected_count: i32,
        processed_count: usize,
        skipped_no_alpha: Vec<String>,
        output_layer: String,
        saved_document: bool,
        outline_offset_mm: f64,
        hole_diameter_mm: f64,
        hole_edge_clearance_mm: f64,
        shapes: Vec<VectorShape>,
    }

    #[derive(Debug, Serialize)]
    struct SelectionVerificationReport {
        schema_version: u32,
        coreldraw_version: String,
        output_sha256: String,
        layer_name: String,
        shape_count: usize,
        shapes: Vec<VerifiedShape>,
    }

    #[derive(Clone)]
    struct Dispatch(IDispatch);

    #[repr(transparent)]
    #[derive(Clone)]
    struct CorelLayerInterface(IUnknown);

    // CorelDRAW 2020's IVGLayer.Import rejects its filename through IDispatch::Invoke.
    unsafe impl Interface for CorelLayerInterface {
        type Vtable = IDispatch_Vtbl;

        const IID: GUID = GUID::from_u128(0xb0580040_9aa4_44fd_9547_4f91eb757ac4);
    }

    impl Dispatch {
        fn id(&self, name: &str) -> Result<i32> {
            let wide = wide_null(name);
            let pointer = PCWSTR(wide.as_ptr());
            let mut id = 0;
            unsafe {
                self.0
                    .GetIDsOfNames(&GUID::zeroed(), &pointer, 1, 0, &mut id)
                    .map_err(|error| com_error(name, error))?;
            }
            Ok(id)
        }

        fn invoke(
            &self,
            name: &str,
            flags: DISPATCH_FLAGS,
            mut args: Vec<VARIANT>,
        ) -> Result<VARIANT> {
            args.reverse();
            let params = DISPPARAMS {
                rgvarg: if args.is_empty() {
                    ptr::null_mut()
                } else {
                    args.as_mut_ptr()
                },
                rgdispidNamedArgs: ptr::null_mut(),
                cArgs: args.len().try_into()?,
                cNamedArgs: 0,
            };
            let mut result = VARIANT::default();
            unsafe {
                self.0
                    .Invoke(
                        self.id(name)?,
                        &GUID::zeroed(),
                        0,
                        flags,
                        &params,
                        Some(&mut result),
                        None,
                        None,
                    )
                    .map_err(|error| com_error(name, error))?;
            }
            Ok(result)
        }

        fn call(&self, name: &str, args: Vec<VARIANT>) -> Result<VARIANT> {
            self.invoke(name, DISPATCH_METHOD, args)
        }

        fn call_dispatch(&self, name: &str, args: Vec<VARIANT>) -> Result<Self> {
            variant_dispatch(self.call(name, args)?)
        }

        fn get(&self, name: &str) -> Result<VARIANT> {
            self.invoke(name, DISPATCH_PROPERTYGET, Vec::new())
        }

        fn get_with_args(&self, name: &str, args: Vec<VARIANT>) -> Result<VARIANT> {
            self.invoke(name, DISPATCH_PROPERTYGET, args)
        }

        fn get_dispatch(&self, name: &str) -> Result<Self> {
            variant_dispatch(self.get(name)?)
        }

        fn get_dispatch_item(&self, index: i32) -> Result<Self> {
            variant_dispatch(self.get_with_args("Item", vec![index.into()])?)
        }

        fn get_string(&self, name: &str) -> Result<String> {
            let value = self.get(name)?;
            Ok(BSTR::try_from(&value)?.to_string())
        }

        fn get_i32(&self, name: &str) -> Result<i32> {
            Ok(i32::try_from(&self.get(name)?)?)
        }

        fn get_f64(&self, name: &str) -> Result<f64> {
            Ok(f64::try_from(&self.get(name)?)?)
        }

        fn get_bool(&self, name: &str) -> Result<bool> {
            Ok(bool::try_from(&self.get(name)?)?)
        }

        fn put(&self, name: &str, value: VARIANT) -> Result<()> {
            let mut args = [value];
            let mut property_put = DISPID_PROPERTYPUT;
            let params = DISPPARAMS {
                rgvarg: args.as_mut_ptr(),
                rgdispidNamedArgs: &mut property_put,
                cArgs: 1,
                cNamedArgs: 1,
            };
            unsafe {
                self.0
                    .Invoke(
                        self.id(name)?,
                        &GUID::zeroed(),
                        0,
                        DISPATCH_PROPERTYPUT,
                        &params,
                        None,
                        None,
                        None,
                    )
                    .map_err(|error| com_error(name, error))?;
            }
            Ok(())
        }
    }

    struct ComApartment;

    impl ComApartment {
        fn initialize() -> Result<Self> {
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .map_err(|error| com_error("CoInitializeEx", error))?;
            }
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub fn run_cli() -> Result<()> {
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        match arguments.as_slice() {
            [command] if command == "probe" => probe(),
            [command] if command == "export-selection-svg" => export_svg_cli(None, true),
            [command, output] if command == "export-selection-svg" => {
                export_svg_cli(Some(Path::new(output)), true)
            }
            [command] if command == "export-page-svg" => export_svg_cli(None, false),
            [command, output] if command == "export-page-svg" => {
                export_svg_cli(Some(Path::new(output)), false)
            }
            [command] if command == "process-selection" => {
                process_selection_cli(
                    ProcessingSettings::default(),
                    SelectionOperation::MergeHoles,
                    None,
                )
            }
            [command, report] if command == "process-selection" => {
                process_selection_cli(
                    ProcessingSettings::default(),
                    SelectionOperation::MergeHoles,
                    Some(Path::new(report)),
                )
            }
            [command, outline_offset, tool, smoothing]
                if command == "outline-selection" =>
            {
                let settings = settings_from_cli(
                    outline_offset,
                    "3.5",
                    "2",
                    Some(parse_mm("道具口径", tool)?),
                    Some(parse_mm("线条平滑", smoothing)?),
                )?;
                process_selection_cli(settings, SelectionOperation::Outline, None)
            }
            [command, diameter, clearance] if command == "add-selection-holes" => {
                let settings = settings_from_cli("2", diameter, clearance, None, None)?;
                process_selection_cli(settings, SelectionOperation::AddHoles, None)
            }
            [command]
                if command == "merge-selected-holes" || command == "merge-selection-holes" =>
            {
                process_selection_cli(
                    ProcessingSettings::default(),
                    SelectionOperation::MergeHoles,
                    None,
                )
            }
            [command, outline_offset, tool, smoothing]
                if command == "trim-transparent-selection" =>
            {
                let settings = settings_from_cli(
                    outline_offset,
                    "3.5",
                    "2",
                    Some(parse_mm("道具口径", tool)?),
                    Some(parse_mm("线条平滑", smoothing)?),
                )?;
                process_selection_cli(settings, SelectionOperation::TrimTransparent, None)
            }
            [command, outline_offset, hole_diameter, hole_edge_clearance]
                if command == "process-selection-settings" =>
            {
                process_selection_cli(
                    settings_from_cli(
                        outline_offset,
                        hole_diameter,
                        hole_edge_clearance,
                        None,
                        None,
                    )?,
                    SelectionOperation::MergeHoles,
                    None,
                )
            }
            [command, outline_offset, hole_diameter, hole_edge_clearance, smoothing_or_report]
                if command == "process-selection-settings" =>
            {
                match parse_mm("线条平滑", smoothing_or_report) {
                    Ok(smoothing_mm) => process_selection_cli(
                        settings_from_cli(
                            outline_offset,
                            hole_diameter,
                            hole_edge_clearance,
                            None,
                            Some(smoothing_mm),
                        )?,
                        SelectionOperation::MergeHoles,
                        None,
                    ),
                    Err(_) => process_selection_cli(
                        settings_from_cli(
                            outline_offset,
                            hole_diameter,
                            hole_edge_clearance,
                            None,
                            None,
                        )?,
                        SelectionOperation::MergeHoles,
                        Some(Path::new(smoothing_or_report)),
                    ),
                }
            }
            [command, outline_offset, hole_diameter, hole_edge_clearance, tool, smoothing]
                if command == "process-selection-settings" =>
            {
                process_selection_cli(
                    settings_from_cli(
                        outline_offset,
                        hole_diameter,
                        hole_edge_clearance,
                        Some(parse_mm("刀具口径", tool)?),
                        Some(parse_mm("线条平滑", smoothing)?),
                    )?,
                    SelectionOperation::MergeHoles,
                    None,
                )
            }
            [command, outline_offset, hole_diameter, hole_edge_clearance, tool, smoothing, report]
                if command == "process-selection-settings" =>
            {
                process_selection_cli(
                    settings_from_cli(
                        outline_offset,
                        hole_diameter,
                        hole_edge_clearance,
                        Some(parse_mm("刀具口径", tool)?),
                        Some(parse_mm("线条平滑", smoothing)?),
                    )?,
                    SelectionOperation::MergeHoles,
                    Some(Path::new(report)),
                )
            }
            [command, source, output, vectors] if command == "write" => write(
                Path::new(source),
                Path::new(output),
                Path::new(vectors),
            ),
            [command, source, output, vectors, report] if command == "verify" => verify(
                Path::new(source),
                Path::new(output),
                Path::new(vectors),
                Path::new(report),
            ),
            [command, output, selection_report, verification_report]
                if command == "verify-selection" =>
            {
                verify_selection(
                    Path::new(output),
                    Path::new(selection_report),
                    Path::new(verification_report),
                )
            }
            _ => Err(io::Error::other(
                "usage: cdr-corel probe | export-selection-svg [output.svg] | export-page-svg [output.svg] | outline-selection <outline-mm> <tool-mm> <smoothing-mm> | add-selection-holes <diameter-mm> <clearance-mm> | merge-selected-holes | trim-transparent-selection <outline-mm> <tool-mm> <smoothing-mm> | process-selection [report.json] | process-selection-settings <outline-mm> <hole-diameter-mm> <hole-edge-clearance-mm> [tool-diameter-mm] [smoothing-mm] [report.json] | verify-selection <output.cdr> <selection-report.json> <verification.json> | write <source.cdr> <output.cdr> <vector-output.json> | verify <source.cdr> <output.cdr> <vector-output.json> <verification.json>",
                )
                .into()),
        }
    }

    fn probe() -> Result<()> {
        let _apartment = ComApartment::initialize()?;
        let app = coreldraw_application()?;
        println!("CorelDRAW {}", app.get_string("Version")?);
        Ok(())
    }

    fn export_svg_cli(output: Option<&Path>, selected: bool) -> Result<()> {
        let _apartment = ComApartment::initialize()?;
        let app = coreldraw_application()?;
        let document = app
            .get_dispatch("ActiveDocument")
            .map_err(|_| io::Error::other("CorelDRAW 中没有活动文档；请先打开文档"))?;
        let output = output
            .map(Path::to_path_buf)
            .unwrap_or(default_svg_output_path(&document)?);
        export_document_svg(&document, &app, &output, selected)?;
        println!("__CDR_EXPORT__\t{}", output.display());
        println!("SVG 导出完成：{}", output.display());
        Ok(())
    }

    fn default_svg_output_path(document: &Dispatch) -> Result<std::path::PathBuf> {
        let desktop = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .map(|path| path.join("Desktop"))
            .ok_or_else(|| io::Error::other("无法定位 Windows 桌面目录"))?;
        let name = document
            .get_string("Name")
            .unwrap_or_else(|_| "transparent-native.cdr".to_string());
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("transparent-native");
        let invalid = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
        let safe_stem = stem
            .chars()
            .map(|character| {
                if invalid.contains(&character) {
                    '_'
                } else {
                    character
                }
            })
            .collect::<String>();
        Ok(desktop.join(format!("{safe_stem}.svg")))
    }

    fn export_document_svg(
        document: &Dispatch,
        app: &Dispatch,
        output: &Path,
        selected: bool,
    ) -> Result<()> {
        if !output.is_absolute() {
            return Err(io::Error::other("SVG 输出路径必须是绝对路径").into());
        }
        if output
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.eq_ignore_ascii_case("svg"))
            != Some(true)
        {
            return Err(io::Error::other("SVG 输出路径必须以 .svg 结尾").into());
        }
        let selection = app.get_dispatch("ActiveSelectionRange")?;
        if selected && selection.get_i32("Count")? == 0 {
            return Err(io::Error::other("导出选中为 SVG 前，请先在 CorelDRAW 中选择对象").into());
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }

        let roots = export_roots(document, app, selected)?;
        let mut bitmaps = Vec::new();
        let mut seen_bitmaps = HashSet::new();
        for root in &roots {
            collect_svg_bitmaps(root, &mut bitmaps, &mut seen_bitmaps)?;
        }

        let temporary = tempfile::Builder::new().prefix("cdr-rust-svg-").tempdir()?;
        let native_path = temporary.path().join("native.svg");
        let tagged_path = temporary.path().join("tagged.svg");
        let document_was_dirty = document.get_bool("Dirty")?;
        let mut renamed = Vec::<usize>::new();
        let export_result = (|| -> Result<String> {
            export_svg_file(document, app, &native_path, selected)?;
            let native_svg = fs::read_to_string(&native_path)?;
            let native_images = inspect_svg_images(&native_svg)?;
            if native_images.len() != bitmaps.len() {
                return Err(io::Error::other(format!(
                    "SVG 位图数量不匹配：Corel 对象 {}，原始 SVG {}；可能包含暂不支持的特效或隐藏对象",
                    bitmaps.len(),
                    native_images.len()
                ))
                .into());
            }
            if bitmaps.iter().all(|bitmap| !bitmap.requires_replacement)
                && native_images.iter().all(|image| image.embedded_data)
            {
                return Ok(native_svg);
            }

            for (index, bitmap) in bitmaps.iter().enumerate() {
                let tagged_name = format!("tsvgMap{:06}", index + 1);
                bitmap.shape.put("Name", tagged_name.as_str().into())?;
                renamed.push(index);
            }
            export_svg_file(document, app, &tagged_path, selected)?;
            restore_svg_bitmap_names(&bitmaps, &renamed)?;
            renamed.clear();
            document.put("Dirty", document_was_dirty.into())?;

            let tagged_svg = fs::read_to_string(&tagged_path)?;
            let tagged_images = inspect_svg_images(&tagged_svg)?;
            if tagged_images.len() != bitmaps.len() {
                return Err(io::Error::other(format!(
                    "SVG 位图数量不匹配：Corel 对象 {}，原始 SVG {}，标记 SVG {}；可能包含暂不支持的特效或隐藏对象",
                    bitmaps.len(), native_images.len(), tagged_images.len()
                ))
                .into());
            }

            let mut bitmap_indexes = Vec::with_capacity(bitmaps.len());
            let mut replacement_required = vec![false; bitmaps.len()];
            let mut seen_indexes = HashSet::new();
            for (native_image, tagged_image) in native_images.iter().zip(&tagged_images) {
                if native_image.geometry != tagged_image.geometry {
                    return Err(io::Error::other(
                        "Corel 原生 SVG 的位图顺序或位置在标记导出时发生变化",
                    )
                    .into());
                }
                let id = tagged_image
                    .id
                    .as_deref()
                    .ok_or_else(|| io::Error::other("无法识别 Corel 导出中的位图 ID"))?;
                let index = id
                    .strip_prefix("tsvgMap")
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|value| (1..=bitmaps.len()).contains(value))
                    .ok_or_else(|| io::Error::other(format!("无法映射 Corel SVG 位图：{id}")))?;
                if !seen_indexes.insert(index) {
                    return Err(io::Error::other("Corel SVG 中出现重复的位图映射 ID").into());
                }
                let bitmap_index = index - 1;
                replacement_required[bitmap_index] =
                    bitmaps[bitmap_index].requires_replacement || !native_image.embedded_data;
                bitmap_indexes.push(bitmap_index);
            }

            let mut pngs = vec![None; bitmaps.len()];
            for (index, bitmap) in bitmaps.iter().enumerate() {
                if !replacement_required[index] {
                    continue;
                }
                let png_path = temporary.path().join(format!("image-{}.png", index + 1));
                export_svg_bitmap(app, document, &selection, bitmap, &png_path)?;
                let png = fs::read(&png_path)?;
                if png.len() < 26
                    || !png.starts_with(b"\x89PNG\r\n\x1a\n")
                    || !matches!(png[25], 4 | 6)
                {
                    return Err(io::Error::other(format!(
                        "第 {} 张位图导出的 PNG 没有保留透明通道",
                        index + 1
                    ))
                    .into());
                }
                pngs[index] = Some(png);
            }
            replace_svg_images_with_pngs(&native_svg, &native_images, &bitmap_indexes, &pngs)
        })();

        let restore_names_result = restore_svg_bitmap_names(&bitmaps, &renamed);
        let restore_selection_result = selection.call("CreateSelection", Vec::new());
        let restore_dirty_result = document.put("Dirty", document_was_dirty.into());
        let svg = export_result?;
        restore_names_result?;
        restore_selection_result?;
        restore_dirty_result?;
        // ExportEx does not consistently replace an existing file across Corel
        // versions.  Writing the final SVG ourselves gives the panel the
        // requested overwrite behavior without touching the CDR.
        fs::write(output, svg)?;
        Ok(())
    }

    fn export_roots(document: &Dispatch, app: &Dispatch, selected: bool) -> Result<Vec<Dispatch>> {
        let collection = if selected {
            app.get_dispatch("ActiveSelectionRange")?
        } else {
            document
                .get_dispatch("ActivePage")?
                .get_dispatch("Shapes")?
        };
        let mut roots = Vec::new();
        for index in 1..=collection.get_i32("Count")? {
            roots.push(collection.get_dispatch_item(index)?);
        }
        if roots.is_empty() {
            return Err(io::Error::other("当前没有可导出的对象").into());
        }
        Ok(roots)
    }

    fn collect_svg_bitmaps(
        shape: &Dispatch,
        bitmaps: &mut Vec<SvgBitmap>,
        seen: &mut HashSet<i32>,
    ) -> Result<()> {
        let shape_type = shape.get_i32("Type")?;
        if shape_type == 0 {
            return Ok(());
        }
        let printable = shape
            .get_dispatch("Layer")
            .and_then(|layer| layer.get_bool("Printable"))
            .unwrap_or(true);
        if !shape.get_bool("Visible")? || !printable {
            return Ok(());
        }
        if shape
            .get_dispatch("Effects")
            .and_then(|effects| effects.get_i32("Count"))
            .unwrap_or(0)
            > 0
            || shape
                .get_dispatch("Transparency")
                .and_then(|transparency| transparency.get_i32("Type"))
                .unwrap_or(0)
                != 0
        {
            return Err(io::Error::other(format!(
                "对象包含暂不支持的特效或透明度：{}",
                shape.get_string("Name").unwrap_or_default()
            ))
            .into());
        }
        if shape_type == 5 {
            let static_id = shape.get_i32("StaticID")?;
            if seen.insert(static_id) {
                bitmaps.push(SvgBitmap {
                    shape: shape.clone(),
                    original_name: shape.get_string("Name")?,
                    requires_replacement: is_inside_powerclip(shape),
                });
            }
            return Ok(());
        }
        if let Ok(children) = shape.get_dispatch("Shapes") {
            for index in 1..=children.get_i32("Count")? {
                collect_svg_bitmaps(&children.get_dispatch_item(index)?, bitmaps, seen)?;
            }
        }
        if let Ok(power_clip) = shape.get_dispatch("PowerClip")
            && let Ok(children) = power_clip.get_dispatch("Shapes")
        {
            for index in 1..=children.get_i32("Count")? {
                collect_svg_bitmaps(&children.get_dispatch_item(index)?, bitmaps, seen)?;
            }
        }
        Ok(())
    }

    fn export_svg_file(
        document: &Dispatch,
        app: &Dispatch,
        path: &Path,
        selected: bool,
    ) -> Result<()> {
        let range = if selected { 2_i32 } else { 1_i32 }; // cdrSelection / cdrCurrentPage
        let export_options = app.call_dispatch("CreateStructExportOptions", Vec::new())?;
        let palette_options = app.call_dispatch("CreateStructPaletteOptions", Vec::new())?;
        let filter = document.call_dispatch(
            "ExportEx",
            vec![
                path_variant(path)?,
                1345_i32.into(), // cdrSVG
                range.into(),
                dispatch_variant(&export_options.0),
                dispatch_variant(&palette_options.0),
            ],
        )?;
        filter.call("Finish", Vec::new())?;
        if !path.is_file() {
            return Err(io::Error::other("CorelDRAW 未生成 SVG 文件").into());
        }
        Ok(())
    }

    fn export_svg_bitmap(
        app: &Dispatch,
        document: &Dispatch,
        original_selection: &Dispatch,
        bitmap: &SvgBitmap,
        path: &Path,
    ) -> Result<()> {
        let shape = &bitmap.shape;
        let was_inside_powerclip = is_inside_powerclip(shape);
        let mut temporary_copy = None;
        let export_result: Result<()> = (|| {
            let export_shape = if was_inside_powerclip {
                let duplicate =
                    shape.call_dispatch("Duplicate", vec![0.0_f64.into(), 0.0_f64.into()])?;
                temporary_copy = Some(duplicate.clone());
                let active_layer = document.get_dispatch("ActiveLayer")?;
                duplicate.call("MoveToLayer", vec![dispatch_variant(&active_layer.0)])?;
                if !same_bitmap_bounds(shape, &duplicate)? {
                    return Err(
                        io::Error::other("PowerClip 位图临时副本的尺寸或位置发生变化").into(),
                    );
                }
                duplicate
            } else {
                shape.clone()
            };
            export_shape.call("CreateSelection", Vec::new())?;
            let bitmap = shape.get_dispatch("Bitmap")?;
            let mut dpi = 300_i32;
            dpi = dpi.max(bitmap.get_i32("ResolutionX")?);
            dpi = dpi.max(bitmap.get_i32("ResolutionY")?);
            if dpi > 1200 {
                dpi = 300;
            }
            let palette_options = app.call_dispatch("CreateStructPaletteOptions", Vec::new())?;
            let export_area = app.call_dispatch(
                "CreateRect",
                vec![
                    0.0_f64.into(),
                    0.0_f64.into(),
                    0.0_f64.into(),
                    0.0_f64.into(),
                ],
            )?;
            let filter = document.call_dispatch(
                "ExportBitmap",
                vec![
                    path_variant(path)?,
                    802_i32.into(),
                    2_i32.into(),
                    4_i32.into(),
                    0_i32.into(),
                    0_i32.into(),
                    dpi.into(),
                    dpi.into(),
                    1_i32.into(),
                    false.into(),
                    true.into(),
                    false.into(),
                    false.into(),
                    0_i32.into(),
                    dispatch_variant(&palette_options.0),
                    dispatch_variant(&export_area.0),
                ],
            )?;
            filter.call("Finish", Vec::new())?;
            Ok(())
        })();
        let cleanup_result = temporary_copy
            .map(|duplicate| duplicate.call("Delete", Vec::new()).map(|_| ()))
            .unwrap_or(Ok(()));
        let restore_result = original_selection.call("CreateSelection", Vec::new());
        export_result?;
        cleanup_result?;
        restore_result?;
        if !path.is_file() {
            return Err(io::Error::other("Corel 未生成透明 PNG").into());
        }
        Ok(())
    }

    fn replace_svg_images_with_pngs(
        svg: &str,
        image_ranges: &[SvgImageTag],
        bitmap_indexes: &[usize],
        pngs: &[Option<Vec<u8>>],
    ) -> Result<String> {
        if image_ranges.len() != bitmap_indexes.len() {
            return Err(io::Error::other("SVG 位图映射数量不匹配").into());
        }
        let mut output = String::with_capacity(svg.len());
        let mut cursor = 0usize;
        for (image, bitmap_index) in image_ranges.iter().zip(bitmap_indexes) {
            let png = pngs
                .get(*bitmap_index)
                .ok_or_else(|| io::Error::other("Corel SVG 位图映射引用了不存在的 PNG"))?;
            output.push_str(&svg[cursor..image.range.start]);
            let tag = &svg[image.range.clone()];
            if let Some(png) = png {
                let placed_tag = place_bitmap_svg_tag(tag, image.parent_matrix)?;
                let encoded = format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(png)
                );
                output.push_str(&replace_svg_href(&placed_tag, &encoded)?);
            } else {
                output.push_str(tag);
            }
            cursor = image.range.end;
        }
        output.push_str(&svg[cursor..]);
        Ok(output)
    }

    fn replace_svg_href(tag: &str, value: &str) -> Result<String> {
        for attribute in ["xlink:href", "href"] {
            if let Some(range) = svg_attribute_range(tag, attribute)? {
                let mut replaced = String::with_capacity(tag.len() + value.len());
                replaced.push_str(&tag[..range.start]);
                replaced.push_str(value);
                replaced.push_str(&tag[range.end..]);
                return Ok(replaced);
            }
        }
        Err(io::Error::other("SVG image 缺少 href 属性").into())
    }

    fn inspect_svg_images(svg: &str) -> Result<Vec<SvgImageTag>> {
        let document = roxmltree::Document::parse_with_options(
            svg,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .map_err(|error| io::Error::other(format!("Corel SVG XML 解析失败：{error}")))?;
        let root_id = document.root_element().id();
        let mut images = Vec::new();
        for node in document.descendants().filter(|node| {
            node.is_element() && node.tag_name().name().eq_ignore_ascii_case("image")
        }) {
            let node_range = node.range();
            let end = svg_tag_end(svg.as_bytes(), node_range.start)?;
            let geometry = [
                "x",
                "y",
                "width",
                "height",
                "transform",
                "preserveAspectRatio",
            ]
            .map(|name| node.attribute(name).map(str::to_owned));
            let embedded_data = node
                .attribute("href")
                .or_else(|| node.attribute(("http://www.w3.org/1999/xlink", "href")))
                .is_some_and(|href| href.trim_start().starts_with("data:image/"));
            let mut ancestors = node
                .ancestors()
                .filter(|ancestor| ancestor.is_element())
                .skip(1)
                .collect::<Vec<_>>();
            ancestors.reverse();
            let mut parent_matrix = identity_matrix();
            for ancestor in ancestors {
                if ancestor.tag_name().name().eq_ignore_ascii_case("svg")
                    && ancestor.id() != root_id
                {
                    return Err(io::Error::other("不支持嵌套 SVG 视口中的位图").into());
                }
                let transform = parse_svg_transform(ancestor.attribute("transform").unwrap_or(""))?;
                parent_matrix = multiply_matrix(parent_matrix, transform);
            }
            images.push(SvgImageTag {
                range: node_range.start..end,
                id: node.attribute("id").map(str::to_owned),
                geometry,
                parent_matrix,
                embedded_data,
            });
        }
        Ok(images)
    }

    fn svg_tag_end(bytes: &[u8], start: usize) -> Result<usize> {
        let mut quote = None;
        for (offset, byte) in bytes.iter().enumerate().skip(start + 1) {
            if let Some(current_quote) = quote {
                if *byte == current_quote {
                    quote = None;
                }
            } else if matches!(*byte, b'"' | b'\'') {
                quote = Some(*byte);
            } else if *byte == b'>' {
                return Ok(offset + 1);
            }
        }
        Err(io::Error::other("SVG image 标签不完整").into())
    }

    fn svg_attribute_range(tag: &str, wanted: &str) -> Result<Option<std::ops::Range<usize>>> {
        let bytes = tag.as_bytes();
        let mut cursor = 1usize;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'/' | b'>')
        {
            cursor += 1;
        }
        while cursor < bytes.len() {
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b'/')
            {
                cursor += 1;
            }
            if cursor >= bytes.len() || bytes[cursor] == b'>' {
                break;
            }
            let name_start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && !matches!(bytes[cursor], b'=' | b'/' | b'>')
            {
                cursor += 1;
            }
            let name_end = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() || bytes[cursor] != b'=' {
                while cursor < bytes.len() && !matches!(bytes[cursor], b'/' | b'>') {
                    cursor += 1;
                }
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() || !matches!(bytes[cursor], b'"' | b'\'') {
                return Err(io::Error::other("SVG 属性引号不完整").into());
            }
            let quote = bytes[cursor];
            let value_start = cursor + 1;
            cursor = value_start;
            while cursor < bytes.len() && bytes[cursor] != quote {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                return Err(io::Error::other("SVG 属性值不完整").into());
            }
            if &tag[name_start..name_end] == wanted {
                return Ok(Some(value_start..cursor));
            }
            cursor += 1;
        }
        Ok(None)
    }

    fn place_bitmap_svg_tag(tag: &str, parent_matrix: [f64; 6]) -> Result<String> {
        let x = svg_attribute_range(tag, "x")?
            .map(|range| parse_svg_number(&tag[range]))
            .transpose()?
            .unwrap_or(0.0);
        let y = svg_attribute_range(tag, "y")?
            .map(|range| parse_svg_number(&tag[range]))
            .transpose()?
            .unwrap_or(0.0);
        let width = svg_attribute_range(tag, "width")?
            .map(|range| parse_svg_number(&tag[range]))
            .transpose()?
            .unwrap_or(0.0);
        let height = svg_attribute_range(tag, "height")?
            .map(|range| parse_svg_number(&tag[range]))
            .transpose()?
            .unwrap_or(0.0);
        if width <= 0.0 || height <= 0.0 {
            return Err(io::Error::other("SVG 位图矩形尺寸无效").into());
        }
        let node_transform = svg_attribute_range(tag, "transform")?
            .map(|range| parse_svg_transform(&tag[range]))
            .transpose()?
            .unwrap_or_else(identity_matrix);
        if matrix_is_identity(parent_matrix) && matrix_is_identity(node_transform) {
            return Ok(tag.to_owned());
        }
        let total = multiply_matrix(parent_matrix, node_transform);
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for (px, py) in [
            (x, y),
            (x + width, y),
            (x, y + height),
            (x + width, y + height),
        ] {
            let (tx, ty) = transform_point(total, px, py);
            min_x = min_x.min(tx);
            min_y = min_y.min(ty);
            max_x = max_x.max(tx);
            max_y = max_y.max(ty);
        }
        let placement = multiply_matrix(
            invert_matrix(parent_matrix)?,
            [1.0, 0.0, 0.0, 1.0, min_x, min_y],
        );
        let matrix = format!(
            "matrix({} {} {} {} {} {})",
            format_svg_number(placement[0]),
            format_svg_number(placement[1]),
            format_svg_number(placement[2]),
            format_svg_number(placement[3]),
            format_svg_number(placement[4]),
            format_svg_number(placement[5]),
        );
        let placed = set_svg_attribute(tag, "x", "0")?;
        let placed = set_svg_attribute(&placed, "y", "0")?;
        let placed = set_svg_attribute(&placed, "width", &format_svg_number(max_x - min_x))?;
        let placed = set_svg_attribute(&placed, "height", &format_svg_number(max_y - min_y))?;
        let placed = set_svg_attribute(&placed, "transform", &matrix)?;
        set_svg_attribute(&placed, "preserveAspectRatio", "none")
    }

    fn set_svg_attribute(tag: &str, name: &str, value: &str) -> Result<String> {
        if let Some(range) = svg_attribute_range(tag, name)? {
            let mut output = String::with_capacity(tag.len() + value.len());
            output.push_str(&tag[..range.start]);
            output.push_str(value);
            output.push_str(&tag[range.end..]);
            return Ok(output);
        }
        let insert_at = tag
            .rfind("/>")
            .or_else(|| tag.rfind('>'))
            .ok_or_else(|| io::Error::other("SVG image 标签不完整"))?;
        let mut output = String::with_capacity(tag.len() + name.len() + value.len() + 4);
        output.push_str(&tag[..insert_at]);
        output.push(' ');
        output.push_str(name);
        output.push_str("=\"");
        output.push_str(value);
        output.push('"');
        output.push_str(&tag[insert_at..]);
        Ok(output)
    }

    fn identity_matrix() -> [f64; 6] {
        [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]
    }

    fn matrix_is_identity(matrix: [f64; 6]) -> bool {
        let identity = identity_matrix();
        matrix
            .into_iter()
            .zip(identity)
            .all(|(actual, expected)| (actual - expected).abs() <= 1.0e-9)
    }

    fn multiply_matrix(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
        [
            a[0] * b[0] + a[2] * b[1],
            a[1] * b[0] + a[3] * b[1],
            a[0] * b[2] + a[2] * b[3],
            a[1] * b[2] + a[3] * b[3],
            a[0] * b[4] + a[2] * b[5] + a[4],
            a[1] * b[4] + a[3] * b[5] + a[5],
        ]
    }

    fn transform_point(matrix: [f64; 6], x: f64, y: f64) -> (f64, f64) {
        (
            matrix[0] * x + matrix[2] * y + matrix[4],
            matrix[1] * x + matrix[3] * y + matrix[5],
        )
    }

    fn invert_matrix(matrix: [f64; 6]) -> Result<[f64; 6]> {
        let determinant = matrix[0] * matrix[3] - matrix[1] * matrix[2];
        if determinant.abs() < 1.0e-12 {
            return Err(io::Error::other("SVG 位图父级变换矩阵不可逆").into());
        }
        Ok([
            matrix[3] / determinant,
            -matrix[1] / determinant,
            -matrix[2] / determinant,
            matrix[0] / determinant,
            (matrix[2] * matrix[5] - matrix[3] * matrix[4]) / determinant,
            (matrix[1] * matrix[4] - matrix[0] * matrix[5]) / determinant,
        ])
    }

    fn parse_svg_transform(raw: &str) -> Result<[f64; 6]> {
        let mut result = identity_matrix();
        let mut cursor = 0usize;
        while cursor < raw.len() {
            let bytes = raw.as_bytes();
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b',')
            {
                cursor += 1;
            }
            if cursor == bytes.len() {
                break;
            }
            let name_start = cursor;
            while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
                cursor += 1;
            }
            if name_start == cursor {
                return Err(io::Error::other("SVG transform 语法不受支持").into());
            }
            let name = &raw[name_start..cursor];
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() || bytes[cursor] != b'(' {
                return Err(io::Error::other("SVG transform 缺少左括号").into());
            }
            cursor += 1;
            let values_start = cursor;
            while cursor < bytes.len() && bytes[cursor] != b')' {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                return Err(io::Error::other("SVG transform 缺少右括号").into());
            }
            let values = parse_svg_numbers(&raw[values_start..cursor])?;
            cursor += 1;
            let part = match name {
                "matrix" if values.len() == 6 => [
                    values[0], values[1], values[2], values[3], values[4], values[5],
                ],
                "translate" if (1..=2).contains(&values.len()) => [
                    1.0,
                    0.0,
                    0.0,
                    1.0,
                    values[0],
                    *values.get(1).unwrap_or(&0.0),
                ],
                "scale" if (1..=2).contains(&values.len()) => [
                    values[0],
                    0.0,
                    0.0,
                    *values.get(1).unwrap_or(&values[0]),
                    0.0,
                    0.0,
                ],
                "rotate" if values.len() == 1 || values.len() == 3 => {
                    let radians = values[0].to_radians();
                    let (sin, cos) = radians.sin_cos();
                    let (cx, cy) = if values.len() == 3 {
                        (values[1], values[2])
                    } else {
                        (0.0, 0.0)
                    };
                    [
                        cos,
                        sin,
                        -sin,
                        cos,
                        cx - cos * cx + sin * cy,
                        cy - sin * cx - cos * cy,
                    ]
                }
                "skewX" if values.len() == 1 => {
                    [1.0, 0.0, values[0].to_radians().tan(), 1.0, 0.0, 0.0]
                }
                "skewY" if values.len() == 1 => {
                    [1.0, values[0].to_radians().tan(), 0.0, 1.0, 0.0, 0.0]
                }
                _ => return Err(io::Error::other("SVG transform 参数数量或类型无效").into()),
            };
            result = multiply_matrix(result, part);
        }
        Ok(result)
    }

    fn parse_svg_numbers(raw: &str) -> Result<Vec<f64>> {
        let bytes = raw.as_bytes();
        let mut values = Vec::new();
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            while cursor < bytes.len()
                && (bytes[cursor].is_ascii_whitespace() || bytes[cursor] == b',')
            {
                cursor += 1;
            }
            if cursor == bytes.len() {
                break;
            }
            let start = cursor;
            if matches!(bytes[cursor], b'+' | b'-') {
                cursor += 1;
            }
            let mut digits = 0usize;
            while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                cursor += 1;
                digits += 1;
            }
            if cursor < bytes.len() && bytes[cursor] == b'.' {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                    digits += 1;
                }
            }
            if digits == 0 {
                return Err(io::Error::other("SVG transform 数值无效").into());
            }
            if cursor < bytes.len() && matches!(bytes[cursor], b'e' | b'E') {
                cursor += 1;
                if cursor < bytes.len() && matches!(bytes[cursor], b'+' | b'-') {
                    cursor += 1;
                }
                let exponent_start = cursor;
                while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }
                if cursor == exponent_start {
                    return Err(io::Error::other("SVG transform 指数无效").into());
                }
            }
            let value = raw[start..cursor]
                .parse::<f64>()
                .map_err(|_| io::Error::other("SVG transform 数值无效"))?;
            if !value.is_finite() {
                return Err(io::Error::other("SVG transform 数值不是有限值").into());
            }
            values.push(value);
        }
        Ok(values)
    }

    fn parse_svg_number(raw: &str) -> Result<f64> {
        let mut values = parse_svg_numbers(raw)?;
        if values.len() != 1 {
            return Err(io::Error::other("SVG 位图坐标不是单一数值").into());
        }
        Ok(values.remove(0))
    }

    fn format_svg_number(value: f64) -> String {
        let mut formatted = format!("{value:.9}");
        while formatted.contains('.') && formatted.ends_with('0') {
            formatted.pop();
        }
        if formatted.ends_with('.') {
            formatted.pop();
        }
        if formatted == "-0" {
            "0".to_owned()
        } else {
            formatted
        }
    }

    fn restore_svg_bitmap_names(bitmaps: &[SvgBitmap], renamed: &[usize]) -> Result<()> {
        let mut first_error = None;
        for index in renamed.iter().rev().copied() {
            if let Some(bitmap) = bitmaps.get(index)
                && let Err(error) = bitmap
                    .shape
                    .put("Name", bitmap.original_name.as_str().into())
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn is_inside_powerclip(shape: &Dispatch) -> bool {
        let mut current = shape.clone();
        for _ in 0..64 {
            if current.get_dispatch("PowerClipParent").is_ok() {
                return true;
            }
            let Ok(parent) = current.get_dispatch("ParentGroup") else {
                return false;
            };
            current = parent;
        }
        false
    }

    fn same_bitmap_bounds(first: &Dispatch, second: &Dispatch) -> Result<bool> {
        for name in ["PositionX", "PositionY", "SizeWidth", "SizeHeight"] {
            if (first.get_f64(name)? - second.get_f64(name)?).abs() > 1.0e-7 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn process_selection_cli(
        settings: ProcessingSettings,
        operation: SelectionOperation,
        report_path: Option<&Path>,
    ) -> Result<()> {
        let outcome = process_selection_with_progress(
            settings,
            operation,
            report_path,
            |progress, stage| println!("__CDR_PROGRESS__\t{progress:.3}\t{stage}"),
        )?;
        if operation == SelectionOperation::MergeHoles {
            println!(
                "{}完成：已融合 {} 组曲线，选择对象 {} 个；未接触的孔 {} 个保持独立，文档未自动保存",
                operation.progress_prefix(),
                outcome.processed_count,
                outcome.selected_count,
                outcome.untouched_hole_count,
            );
        } else {
            println!(
                "{}完成：已处理 {}/{} 张位图，输出图层 {}；文档未自动保存",
                operation.progress_prefix(),
                outcome.processed_count,
                outcome.selected_count,
                outcome.output_layer
            );
        }
        if !outcome.skipped_no_alpha.is_empty() {
            println!(
                "跳过 {} 张无透明边缘位图: {}",
                outcome.skipped_no_alpha.len(),
                outcome.skipped_no_alpha.join(", ")
            );
        }
        Ok(())
    }

    fn settings_from_cli(
        outline_offset: impl AsRef<std::ffi::OsStr>,
        hole_diameter: impl AsRef<std::ffi::OsStr>,
        hole_edge_clearance: impl AsRef<std::ffi::OsStr>,
        tool_diameter_mm: Option<f64>,
        smoothing_mm: Option<f64>,
    ) -> Result<ProcessingSettings> {
        Ok(ProcessingSettings {
            outline_offset_mm: parse_mm("巡边外扩", outline_offset.as_ref())?,
            hole_diameter_mm: parse_mm("孔径", hole_diameter.as_ref())?,
            hole_edge_clearance_mm: parse_mm("孔边距", hole_edge_clearance.as_ref())?,
            tool_diameter_mm: tool_diameter_mm.unwrap_or(TOOL_DIAMETER_MM),
            smoothing_mm: smoothing_mm.unwrap_or(SMOOTHING_MM),
        })
    }

    fn parse_mm(name: &str, value: &std::ffi::OsStr) -> Result<f64> {
        value
            .to_str()
            .ok_or_else(|| io::Error::other(format!("{name} 参数不是有效文本")))?
            .parse::<f64>()
            .map_err(|_| io::Error::other(format!("{name} 参数不是有效数字")).into())
    }

    pub fn process_active_selection(
        settings: ProcessingSettings,
        report_path: Option<&Path>,
    ) -> Result<ProcessOutcome> {
        process_selection_with_progress(
            settings,
            SelectionOperation::MergeHoles,
            report_path,
            |_, _| {},
        )
    }

    pub fn process_selection_with_progress<F>(
        settings: ProcessingSettings,
        operation: SelectionOperation,
        report_path: Option<&Path>,
        mut progress: F,
    ) -> Result<ProcessOutcome>
    where
        F: FnMut(f32, &str),
    {
        if operation != SelectionOperation::MergeHoles {
            settings.validate()?;
        }
        progress(0.02, "连接 CorelDRAW 并读取当前选择");
        let _apartment = ComApartment::initialize()?;
        let app = coreldraw_application()?;
        let version = app.get_string("Version")?;
        let document = app
            .get_dispatch("ActiveDocument")
            .map_err(|_| io::Error::other("CorelDRAW 中没有活动文档；请先打开文档并选择位图"))?;
        let page = document.get_dispatch("ActivePage")?;
        let selection = app.get_dispatch("ActiveSelectionRange")?;
        let selected_count = selection.get_i32("Count")?;
        if selected_count == 0 {
            return Err(io::Error::other("当前没有选择对象；请先选择要处理的内容").into());
        }

        if operation == SelectionOperation::MergeHoles {
            return merge_selected_holes(
                &document,
                &selection,
                selected_count,
                report_path,
                &mut progress,
            );
        }

        let units = document_units(&app, &document)?;
        let temporary = tempfile::Builder::new()
            .prefix("cdr-rust-selection-")
            .tempdir()?;
        let mut vector_shapes = Vec::new();
        let mut skipped_no_alpha = Vec::new();
        let mut replacements = Vec::new();
        let mut processed_static_ids = Vec::new();

        // After “巡边”, the user normally selects the generated outline curve
        // before clicking “加孔”.  Resolve that curve back to its source bitmap
        // so both operation orders use the same raster contour logic.  Mixed
        // selections are intentionally supported: non-bitmap objects are ignored
        // unless they are one of our generated outline curves.
        let mut bitmap_sources = Vec::<(Dispatch, i32)>::new();
        let mut seen_source_ids = HashSet::new();
        for index in 1..=selected_count {
            let shape = selection.get_dispatch_item(index)?;
            let shape_type = shape.get_i32("Type")?;
            let source = if shape_type == 5 {
                Some(shape)
            } else if operation == SelectionOperation::AddHoles && matches!(shape_type, 3 | 7) {
                let name = shape.get_string("Name")?;
                if let Some(static_id) = parse_generated_outline_id(&name) {
                    Some(find_bitmap_by_static_id(&page, static_id)?.ok_or_else(|| {
                        io::Error::other(format!(
                            "找不到巡边曲线对应的原始位图（StaticID {static_id}）；请保留原始位图后再加孔"
                        ))
                    })?)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(source) = source {
                let static_id = source.get_i32("StaticID")?;
                if seen_source_ids.insert(static_id) {
                    bitmap_sources.push((source, static_id));
                }
            }
        }

        if bitmap_sources.is_empty() {
            return Err(io::Error::other(match operation {
                SelectionOperation::AddHoles => {
                    "加孔需要选择位图或已生成的巡边曲线；当前选择中没有可用来源"
                }
                _ => "当前选择中没有可处理的位图",
            })
            .into());
        }

        let source_count = bitmap_sources.len();
        for (source_index, (shape, static_id)) in bitmap_sources.iter().enumerate() {
            let index = i32::try_from(source_index + 1)?;
            progress(
                0.08 + 0.62 * source_index as f32 / source_count as f32,
                &format!(
                    "{}：读取第 {index}/{source_count} 张位图",
                    operation.progress_prefix()
                ),
            );
            validate_selected_bitmap_transform(shape, index)?;
            let bitmap = shape.get_dispatch("Bitmap")?;
            if bitmap.get_bool("Cropped")? {
                return Err(io::Error::other(format!(
                    "选择中的第 {index} 张位图已裁剪；当前版本暂不处理裁剪位图"
                ))
                .into());
            }

            let source_id = format!("selection.{index}.id-{static_id}");
            let png_path = temporary.path().join(format!("bitmap-{index}.png"));
            let expected_width = u32::try_from(bitmap.get_i32("SizeWidth")?)?;
            let expected_height = u32::try_from(bitmap.get_i32("SizeHeight")?)?;
            let resolution_x = bitmap.get_i32("ResolutionX")?;
            let resolution_y = bitmap.get_i32("ResolutionY")?;
            export_bitmap_png(
                &app,
                &document,
                &selection,
                shape,
                &png_path,
                expected_width.try_into()?,
                expected_height.try_into()?,
                resolution_x,
                resolution_y,
            )?;
            let image = ImageReader::open(&png_path)?.decode()?.into_rgba8();
            if image.width() != expected_width || image.height() != expected_height {
                return Err(io::Error::other(format!(
                    "{source_id} 导出尺寸异常：预期 {expected_width}x{expected_height}，实际 {}x{}",
                    image.width(),
                    image.height()
                ))
                .into());
            }

            let alpha = image.pixels().map(|pixel| pixel.0[3]).collect();
            let mask = AlphaMask::try_new(image.width(), image.height(), alpha)?;
            if !mask.stats().has_transparent_edge() {
                skipped_no_alpha.push(source_id);
                continue;
            }

            let bounds = BoundsMm {
                left: units.to_mm(shape.get_f64("LeftX")?),
                bottom: units.to_mm(shape.get_f64("BottomY")?),
                width: units.to_mm(shape.get_f64("SizeWidth")?),
                height: units.to_mm(shape.get_f64("SizeHeight")?),
            };
            if operation == SelectionOperation::TrimTransparent {
                let (trimmed, trimmed_bounds) = trim_rgba_image(&image, &mask, bounds)?;
                let trimmed_alpha = trimmed.pixels().map(|pixel| pixel.0[3]).collect();
                let trimmed_mask =
                    AlphaMask::try_new(trimmed.width(), trimmed.height(), trimmed_alpha)?;
                let contour = trimmed_mask.trace_contour(
                    trimmed_bounds.width,
                    trimmed_bounds.height,
                    OutlineSettings {
                        alpha_threshold: TRIM_ALPHA_THRESHOLD,
                        offset_mm: 0.0,
                        tool_diameter_mm: 0.0,
                        smoothing_mm: settings.smoothing_mm,
                        minimum_component_area_mm2: MINIMUM_COMPONENT_AREA_MM2,
                    },
                )?;
                let trimmed_path = temporary.path().join(format!("trimmed-{index}.png"));
                trimmed.save(&trimmed_path)?;
                replacements.push(BitmapReplacement {
                    source: shape.clone(),
                    source_static_id: *static_id,
                    png_path: trimmed_path,
                    bounds: trimmed_bounds,
                });
                vector_shapes.push(VectorShape {
                    source_shape_path: source_id,
                    name: generated_shape_name(operation, *static_id),
                    outline_hex: "#EC2A90".to_string(),
                    outline_width_mm: 0.076,
                    polygons: to_page_polygons(&contour, trimmed_bounds, None)?,
                });
                processed_static_ids.push(*static_id);
                continue;
            }

            let base_outline = mask.offset_outline(
                bounds.width,
                bounds.height,
                OutlineSettings {
                    alpha_threshold: ALPHA_THRESHOLD,
                    offset_mm: settings.outline_offset_mm,
                    tool_diameter_mm: settings.tool_diameter_mm,
                    smoothing_mm: settings.smoothing_mm,
                    minimum_component_area_mm2: MINIMUM_COMPONENT_AREA_MM2,
                },
            )?;
            let hole_settings = HolePlacementSettings {
                diameter_mm: settings.hole_diameter_mm,
                edge_clearance_mm: settings.hole_edge_clearance_mm,
                smoothing_mm: settings.smoothing_mm,
            };
            let (geometry, page_bounds, curved_geometry) = match operation {
                SelectionOperation::Outline => (base_outline, bounds, None),
                SelectionOperation::AddHoles => {
                    if settings.hole_diameter_mm == 0.0 {
                        return Err(io::Error::other("孔径为 0 mm，未生成孔位").into());
                    }
                    let ring = add_top_center_hole_ring(&base_outline, hole_settings)?;
                    (ring.geometry, bounds, ring.curved_geometry)
                }
                SelectionOperation::MergeHoles => {
                    let combined = combine_top_center_hole(&base_outline, hole_settings)?;
                    (combined.geometry, bounds, combined.curved_geometry)
                }
                SelectionOperation::TrimTransparent => unreachable!("handled above"),
            };
            let marker = generated_shape_name(operation, *static_id);
            vector_shapes.push(VectorShape {
                source_shape_path: source_id,
                name: marker,
                outline_hex: "#EC2A90".to_string(),
                outline_width_mm: 0.076,
                polygons: to_page_polygons(&geometry, page_bounds, curved_geometry.as_deref())?,
            });
            processed_static_ids.push(*static_id);
        }

        if vector_shapes.is_empty() && replacements.is_empty() {
            return Err(
                io::Error::other("所选位图都没有可用的透明像素边缘，未对文档进行任何修改").into(),
            );
        }

        progress(0.74, "正在准备 CorelDRAW 写回");
        let output_layer = if vector_shapes.is_empty() {
            "透明边缘已写入图片".to_owned()
        } else {
            unique_layer_name(&page, operation.layer_name())?
        };
        let document_name = document.get_string("Name")?;
        let outcome = ProcessOutcome {
            document_name: document_name.clone(),
            selected_count,
            processed_count: processed_static_ids.len(),
            untouched_hole_count: 0,
            skipped_no_alpha: skipped_no_alpha.clone(),
            output_layer: output_layer.clone(),
            operation,
        };
        let mut report = SelectionReport {
            schema_version: 1,
            coreldraw_version: version,
            document_name,
            page_index: page.get_i32("Index")?,
            selected_count,
            processed_count: vector_shapes.len(),
            skipped_no_alpha,
            output_layer: output_layer.clone(),
            saved_document: false,
            outline_offset_mm: settings.outline_offset_mm,
            hole_diameter_mm: settings.hole_diameter_mm,
            hole_edge_clearance_mm: settings.hole_edge_clearance_mm,
            shapes: vector_shapes,
        };

        document.call("BeginCommandGroup", vec!["Rust 处理当前选择".into()])?;
        let write_result = (|| {
            for static_id in &processed_static_ids {
                remove_generated_shapes(page.clone(), *static_id, operation)?;
            }
            let layer = if report.shapes.is_empty() {
                None
            } else {
                Some(page.call_dispatch("CreateLayer", vec![output_layer.as_str().into()])?)
            };
            let mut trimmed_bitmaps = Vec::with_capacity(replacements.len());
            for replacement in &replacements {
                let layer = layer
                    .as_ref()
                    .ok_or_else(|| io::Error::other("透明边缘矢量图层尚未创建"))?;
                let imported = import_trimmed_bitmap(&app, layer, replacement, units)?;
                let imported_static_id = imported.get_i32("StaticID")?;
                let previous_name = generated_shape_name(
                    SelectionOperation::TrimTransparent,
                    replacement.source_static_id,
                );
                if let Some(outline) = report
                    .shapes
                    .iter_mut()
                    .find(|shape| shape.name == previous_name)
                {
                    outline.name = generated_shape_name(
                        SelectionOperation::TrimTransparent,
                        imported_static_id,
                    );
                }
                trimmed_bitmaps.push((imported, imported_static_id));
            }
            if let Some(layer) = &layer {
                write_shapes(&app, &document, layer, &report.shapes, units)?;
                verify_layer_shapes(layer, &report.shapes, units)?;

                let shapes = layer.get_dispatch("Shapes")?;
                let mut composite_groups = Vec::with_capacity(trimmed_bitmaps.len());
                for (bitmap, static_id) in &trimmed_bitmaps {
                    let outline_name =
                        generated_shape_name(SelectionOperation::TrimTransparent, *static_id);
                    let outline = (1..=shapes.get_i32("Count")?)
                        .map(|index| shapes.get_dispatch_item(index))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .find(|shape| {
                            shape.get_string("Name").ok().as_deref() == Some(outline_name.as_str())
                        })
                        .ok_or_else(|| {
                            io::Error::other(format!("找不到透明边矢量曲线：{outline_name}"))
                        })?;
                    bitmap.call("CreateSelection", Vec::new())?;
                    outline.call("AddToSelection", Vec::new())?;
                    let selected_pair = app.get_dispatch("ActiveSelectionRange")?;
                    if selected_pair.get_i32("Count")? != 2 {
                        return Err(io::Error::other("无法将处理后的位图和矢量边组成一组").into());
                    }
                    let group = selected_pair.call_dispatch("Group", Vec::new())?;
                    group.put("Name", outline_name.as_str().into())?;
                    composite_groups.push(group);
                }
                for (index, group) in composite_groups.iter().enumerate() {
                    if index == 0 {
                        group.call("CreateSelection", Vec::new())?;
                    } else {
                        group.call("AddToSelection", Vec::new())?;
                    }
                }
            }
            if let Some(path) = report_path {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, serde_json::to_vec_pretty(&report)?)?;
            }
            document.call("EndCommandGroup", Vec::new())?;
            if operation != SelectionOperation::TrimTransparent {
                selection.call("CreateSelection", Vec::new())?;
            }
            Ok(())
        })();

        if let Err(error) = write_result {
            let _ = document.call("EndCommandGroup", Vec::new());
            let _ = document.call("Undo", vec![1_i32.into()]);
            let _ = selection.call("CreateSelection", Vec::new());
            return Err(error);
        }

        progress(1.0, "处理完成；CorelDRAW 文档未自动保存");
        Ok(outcome)
    }

    #[derive(Serialize)]
    struct MergeSelectionReport {
        schema_version: u32,
        document_name: String,
        selected_count: i32,
        merged_group_count: usize,
        untouched_hole_count: usize,
        saved_document: bool,
    }

    fn merge_selected_holes<F>(
        document: &Dispatch,
        selection: &Dispatch,
        selected_count: i32,
        report_path: Option<&Path>,
        progress: &mut F,
    ) -> Result<ProcessOutcome>
    where
        F: FnMut(f32, &str),
    {
        let app = coreldraw_application()?;
        let units = document_units(&app, document)?;
        let mut curves = Vec::with_capacity(selected_count as usize);
        for index in 1..=selected_count {
            progress(
                0.05 + 0.08 * (index - 1) as f32 / selected_count as f32,
                &format!("读取所选曲线 {index}/{selected_count}"),
            );
            let shape = selection.get_dispatch_item(index)?;
            let shape_type = shape.get_i32("Type")?;
            let mut candidates = Vec::<(Dispatch, Option<Dispatch>)>::new();
            if shape_type == 3 {
                candidates.push((shape, None));
            } else if shape_type == 7
                && parse_generated_outline_id(&shape.get_string("Name")?).is_some()
            {
                let children = shape.get_dispatch("Shapes")?;
                for child_index in 1..=children.get_i32("Count")? {
                    let child = children.get_dispatch_item(child_index)?;
                    if child.get_i32("Type")? == 3
                        && (parse_generated_outline_id(&child.get_string("Name")?).is_some()
                            || child.get_string("Name")?.starts_with("CDR_RUST_MERGED_"))
                    {
                        candidates.push((child, Some(shape.clone())));
                    }
                }
            }
            for (shape, parent_group) in candidates {
                let name = shape.get_string("Name")?;
                let is_hole = name.starts_with("CDR_RUST_HOLE_");
                let curve = shape.get_dispatch("Curve")?;
                curves.push(MergeSelectionShape {
                    inner_rings: if is_hole {
                        capture_inner_rings(&curve, units)?
                    } else {
                        Vec::new()
                    },
                    curve,
                    parent_group,
                    is_hole,
                    shape,
                });
            }
        }

        let outline_indices = curves
            .iter()
            .enumerate()
            .filter_map(|(index, shape)| (!shape.is_hole).then_some(index))
            .collect::<Vec<_>>();
        let hole_indices = curves
            .iter()
            .enumerate()
            .filter_map(|(index, shape)| shape.is_hole.then_some(index))
            .collect::<Vec<_>>();
        if outline_indices.is_empty() || hole_indices.is_empty() {
            return Err(io::Error::other(
                "请同时选择至少一条内容巡边曲线和一个通过“加孔”生成的孔位曲线。",
            )
            .into());
        }

        let pair_count = outline_indices.len() * hole_indices.len();
        let mut checked_pairs = 0usize;
        let mut adjacency = vec![Vec::<usize>::new(); curves.len()];
        for outline_index in outline_indices.iter().copied() {
            for hole_index in hole_indices.iter().copied() {
                checked_pairs += 1;
                progress(
                    0.14 + 0.26 * checked_pairs as f32 / pair_count as f32,
                    &format!("检查孔位与轮廓是否接触 {checked_pairs}/{pair_count}"),
                );
                if curves_intersect(&curves[outline_index].curve, &curves[hole_index].curve)? {
                    adjacency[outline_index].push(hole_index);
                    adjacency[hole_index].push(outline_index);
                }
            }
        }

        if !adjacency.iter().any(|neighbors| !neighbors.is_empty()) {
            return Err(io::Error::other(
                "所选孔位没有与任何巡边曲线接触，未修改文档；请检查孔位是否贴到轮廓。",
            )
            .into());
        }

        let untouched_hole_count = hole_indices
            .iter()
            .filter(|index| adjacency[**index].is_empty())
            .count();
        let mut visited = vec![false; curves.len()];
        let mut merge_groups = Vec::<Vec<usize>>::new();
        for start in 0..curves.len() {
            if visited[start] || adjacency[start].is_empty() {
                continue;
            }
            let mut stack = vec![start];
            let mut component = Vec::new();
            visited[start] = true;
            while let Some(current) = stack.pop() {
                component.push(current);
                for neighbor in adjacency[current].iter().copied() {
                    if !visited[neighbor] {
                        visited[neighbor] = true;
                        stack.push(neighbor);
                    }
                }
            }
            if component.iter().any(|index| !curves[*index].is_hole)
                && component.iter().any(|index| curves[*index].is_hole)
            {
                component.sort_by_key(|index| curves[*index].is_hole);
                merge_groups.push(component);
            }
        }

        if merge_groups.is_empty() {
            return Err(io::Error::other("没有找到可融合的轮廓与孔位组合。未修改文档。").into());
        }

        let document_name = document.get_string("Name")?;
        progress(0.42, "接触检测完成，正在执行 CorelDRAW 曲线焊接");
        document.call("BeginCommandGroup", vec!["Rust 合并孔位".into()])?;
        let merge_result = (|| -> Result<()> {
            for (group_index, component) in merge_groups.iter().enumerate() {
                let mut ordered = component.clone();
                let base_position = ordered
                    .iter()
                    .position(|index| !curves[*index].is_hole)
                    .ok_or_else(|| io::Error::other("融合组缺少轮廓曲线"))?;
                ordered.swap(0, base_position);
                let mut welded = curves[ordered[0]].shape.clone();
                for (shape_index, source_index) in ordered.iter().copied().skip(1).enumerate() {
                    progress(
                        0.46 + 0.48
                            * (group_index as f32 + shape_index as f32 / ordered.len() as f32)
                            / merge_groups.len() as f32,
                        &format!(
                            "融合第 {}/{} 组中的曲线 {}/{}",
                            group_index + 1,
                            merge_groups.len(),
                            shape_index + 2,
                            ordered.len()
                        ),
                    );
                    if curves[source_index].is_hole {
                        retain_outer_ring(&curves[source_index].curve)?;
                    }
                    welded = welded.call_dispatch(
                        "Weld",
                        vec![
                            dispatch_variant(&curves[source_index].shape.0),
                            false.into(),
                            false.into(),
                        ],
                    )?;
                }
                let welded_curve = welded.get_dispatch("Curve")?;
                for ring in ordered
                    .iter()
                    .flat_map(|index| curves[*index].inner_rings.iter())
                {
                    append_closed_path(&welded_curve, ring, units)?;
                }
                let static_id = welded.get_i32("StaticID")?;
                let output_name = format!("CDR_RUST_MERGED_{static_id}");
                welded.put("Name", output_name.as_str().into())?;

                if let Some(parent_group) = &curves[ordered[0]].parent_group {
                    let composite_name = parent_group.get_string("Name")?;
                    if parse_generated_outline_id(&composite_name).is_none() {
                        return Err(io::Error::other(
                            "合并复合对象中的孔位失败：无法识别图片轮廓组",
                        )
                        .into());
                    }
                    parent_group.call("CreateSelection", Vec::new())?;
                    welded.call("AddToSelection", Vec::new())?;
                    let selected_shapes = app.get_dispatch("ActiveSelectionRange")?;
                    if selected_shapes.get_i32("Count")? != 2 {
                        return Err(io::Error::other("无法将合并后的曲线放回图片复合组").into());
                    }
                    let composite = selected_shapes.call_dispatch("Group", Vec::new())?;
                    composite.put("Name", composite_name.as_str().into())?;
                }
            }

            if let Some(path) = report_path {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let report = MergeSelectionReport {
                    schema_version: 1,
                    document_name: document_name.clone(),
                    selected_count,
                    merged_group_count: merge_groups.len(),
                    untouched_hole_count,
                    saved_document: false,
                };
                fs::write(path, serde_json::to_vec_pretty(&report)?)?;
            }
            document.call("EndCommandGroup", Vec::new())?;
            Ok(())
        })();

        if let Err(error) = merge_result {
            let _ = document.call("EndCommandGroup", Vec::new());
            let _ = document.call("Undo", vec![1_i32.into()]);
            return Err(error);
        }

        progress(1.0, "孔位融合完成；未接触的孔保持独立，文档未自动保存");
        Ok(ProcessOutcome {
            document_name,
            selected_count,
            processed_count: merge_groups.len(),
            untouched_hole_count,
            skipped_no_alpha: Vec::new(),
            output_layer: "Corel 原生焊接结果".to_string(),
            operation: SelectionOperation::MergeHoles,
        })
    }

    fn curves_intersect(first: &Dispatch, second: &Dispatch) -> Result<bool> {
        let result = first.call("IntersectsWith", vec![dispatch_variant(&second.0)])?;
        Ok(bool::try_from(&result)?)
    }

    fn capture_inner_rings(curve: &Dispatch, units: DocumentUnits) -> Result<Vec<ClosedPath>> {
        let subpaths = curve.get_dispatch("SubPaths")?;
        let mut rings = Vec::new();
        for index in 1..=subpaths.get_i32("Count")? {
            let subpath = subpaths.get_dispatch_item(index)?;
            if !bool::try_from(&subpath.get("Closed")?)? {
                continue;
            }
            let segments = subpath.get_dispatch("Segments")?;
            let mut path_segments = Vec::with_capacity(segments.get_i32("Count")? as usize);
            for segment_index in 1..=segments.get_i32("Count")? {
                let segment = segments.get_dispatch_item(segment_index)?;
                let start_node = segment.get_dispatch("StartNode")?;
                let end_node = segment.get_dispatch("EndNode")?;
                let start = DomainPointMm {
                    x: units.to_mm(start_node.get_f64("PositionX")?),
                    y: units.to_mm(start_node.get_f64("PositionY")?),
                };
                let end = DomainPointMm {
                    x: units.to_mm(end_node.get_f64("PositionX")?),
                    y: units.to_mm(end_node.get_f64("PositionY")?),
                };
                match segment.get_i32("Type")? {
                    0 => path_segments.push(DomainPathSegment::Line { start, end }),
                    1 => path_segments.push(DomainPathSegment::CubicBezier {
                        start,
                        control_start: DomainPointMm {
                            x: units.to_mm(segment.get_f64("StartingControlPointX")?),
                            y: units.to_mm(segment.get_f64("StartingControlPointY")?),
                        },
                        control_end: DomainPointMm {
                            x: units.to_mm(segment.get_f64("EndingControlPointX")?),
                            y: units.to_mm(segment.get_f64("EndingControlPointY")?),
                        },
                        end,
                    }),
                    segment_type => {
                        return Err(io::Error::other(format!(
                            "unsupported Corel segment type: {segment_type}"
                        ))
                        .into());
                    }
                }
            }
            if path_segments.len() >= 3 {
                let path = ClosedPath::try_new(path_segments)?;
                let points = path
                    .segments()
                    .iter()
                    .map(|segment| {
                        let point = segment.start();
                        (point.x, point.y)
                    })
                    .collect::<Vec<_>>();
                rings.push((ring_area(&points), path));
            }
        }

        let outer_index = rings
            .iter()
            .enumerate()
            .max_by(|(_, first), (_, second)| first.0.total_cmp(&second.0))
            .map(|(index, _)| index)
            .ok_or_else(|| io::Error::other("孔位曲线没有有效的闭合外圈"))?;
        if rings.len() < 2 {
            return Err(
                io::Error::other("孔位曲线缺少中心圆子路径；请重新使用“加孔”生成孔位").into(),
            );
        }

        Ok(rings
            .into_iter()
            .enumerate()
            .filter_map(|(index, (_, path))| (index != outer_index).then_some(path))
            .collect())
    }

    fn retain_outer_ring(curve: &Dispatch) -> Result<()> {
        let subpaths = curve.get_dispatch("SubPaths")?;
        let count = subpaths.get_i32("Count")?;
        let mut areas = Vec::new();
        for index in 1..=count {
            let subpath = subpaths.get_dispatch_item(index)?;
            if !bool::try_from(&subpath.get("Closed")?)? {
                continue;
            }
            let nodes = subpath.get_dispatch("Nodes")?;
            let mut points = Vec::with_capacity(nodes.get_i32("Count")? as usize);
            for node_index in 1..=nodes.get_i32("Count")? {
                let node = nodes.get_dispatch_item(node_index)?;
                points.push((node.get_f64("PositionX")?, node.get_f64("PositionY")?));
            }
            if points.len() >= 3 {
                areas.push((index, ring_area(&points)));
            }
        }
        let outer_index = areas
            .iter()
            .max_by(|(_, first_area), (_, second_area)| first_area.total_cmp(second_area))
            .map(|(index, _)| *index)
            .ok_or_else(|| io::Error::other("孔位曲线没有有效的闭合外圈"))?;
        for index in (1..=count).rev() {
            if index != outer_index {
                subpaths
                    .get_dispatch_item(index)?
                    .call("Delete", Vec::new())?;
            }
        }
        Ok(())
    }

    fn ring_area(points: &[(f64, f64)]) -> f64 {
        let mut twice_area = 0.0;
        for index in 0..points.len() {
            let (x1, y1) = points[index];
            let (x2, y2) = points[(index + 1) % points.len()];
            twice_area += x1 * y2 - x2 * y1;
        }
        twice_area.abs() / 2.0
    }

    fn generated_shape_name(operation: SelectionOperation, static_id: i32) -> String {
        let suffix = match operation {
            SelectionOperation::Outline => "OUTLINE",
            SelectionOperation::AddHoles => "HOLE",
            SelectionOperation::MergeHoles => "MERGED",
            SelectionOperation::TrimTransparent => "TRIMMED_OUTLINE",
        };
        format!("CDR_RUST_{suffix}_{static_id}")
    }

    fn parse_generated_outline_id(name: &str) -> Option<i32> {
        ["CDR_RUST_OUTLINE_", "CDR_RUST_TRIMMED_OUTLINE_"]
            .iter()
            .find_map(|prefix| {
                name.strip_prefix(prefix)
                    .and_then(|suffix| suffix.parse::<i32>().ok())
            })
    }

    fn find_bitmap_by_static_id(page: &Dispatch, static_id: i32) -> Result<Option<Dispatch>> {
        let layers = page.get_dispatch("Layers")?;
        for layer_index in 1..=layers.get_i32("Count")? {
            let layer = layers.get_dispatch_item(layer_index)?;
            let shapes = layer.get_dispatch("Shapes")?;
            if let Some(shape) = find_bitmap_in_shapes(&shapes, static_id)? {
                return Ok(Some(shape));
            }
        }
        Ok(None)
    }

    fn find_bitmap_in_shapes(shapes: &Dispatch, static_id: i32) -> Result<Option<Dispatch>> {
        for shape_index in 1..=shapes.get_i32("Count")? {
            let shape = shapes.get_dispatch_item(shape_index)?;
            let shape_type = shape.get_i32("Type")?;
            if shape_type == 5 && shape.get_i32("StaticID")? == static_id {
                return Ok(Some(shape));
            }
            if matches!(shape_type, 3 | 7) {
                if let Ok(children) = shape.get_dispatch("Shapes")
                    && let Some(found) = find_bitmap_in_shapes(&children, static_id)?
                {
                    return Ok(Some(found));
                }
                if let Ok(power_clip) = shape.get_dispatch("PowerClip")
                    && let Ok(children) = power_clip.get_dispatch("Shapes")
                    && let Some(found) = find_bitmap_in_shapes(&children, static_id)?
                {
                    return Ok(Some(found));
                }
            }
        }
        Ok(None)
    }

    fn remove_generated_shapes(
        page: Dispatch,
        static_id: i32,
        operation: SelectionOperation,
    ) -> Result<()> {
        let kinds: &[SelectionOperation] = match operation {
            SelectionOperation::Outline => {
                &[SelectionOperation::Outline, SelectionOperation::MergeHoles]
            }
            SelectionOperation::AddHoles => &[SelectionOperation::AddHoles],
            SelectionOperation::MergeHoles => &[
                SelectionOperation::Outline,
                SelectionOperation::AddHoles,
                SelectionOperation::MergeHoles,
            ],
            SelectionOperation::TrimTransparent => &[
                SelectionOperation::Outline,
                SelectionOperation::AddHoles,
                SelectionOperation::MergeHoles,
                SelectionOperation::TrimTransparent,
            ],
        };
        let names = kinds
            .iter()
            .map(|kind| generated_shape_name(*kind, static_id))
            .collect::<Vec<_>>();
        let layers = page.get_dispatch("Layers")?;
        for layer_index in (1..=layers.get_i32("Count")?).rev() {
            let layer = layers.get_dispatch_item(layer_index)?;
            let shapes = layer.get_dispatch("Shapes")?;
            for shape_index in (1..=shapes.get_i32("Count")?).rev() {
                let shape = shapes.get_dispatch_item(shape_index)?;
                let current_name = shape.get_string("Name");
                if current_name
                    .as_ref()
                    .is_ok_and(|current_name| names.iter().any(|name| name == current_name))
                {
                    shape.call("Delete", Vec::new())?;
                }
            }
        }
        Ok(())
    }

    fn trim_rgba_image(
        image: &RgbaImage,
        mask: &AlphaMask,
        original_bounds: BoundsMm,
    ) -> Result<(RgbaImage, BoundsMm)> {
        let bounds = mask
            .content_bounds_above(TRIM_ALPHA_THRESHOLD)
            .ok_or_else(|| io::Error::other("位图没有高于透明阈值的可见像素"))?;
        let crop_width = bounds.right - bounds.left + 1;
        let crop_height = bounds.bottom - bounds.top + 1;
        let cropped =
            imageops::crop_imm(image, bounds.left, bounds.top, crop_width, crop_height).to_image();
        let pixels_per_mm = (crop_width as f64 / original_bounds.width)
            .min(crop_height as f64 / original_bounds.height);
        if !pixels_per_mm.is_finite() || pixels_per_mm <= 0.0 {
            return Err(io::Error::other("透明边裁切后无法计算等比缩放尺寸").into());
        }
        let scale_mm_per_pixel = 1.0 / pixels_per_mm;
        let trimmed_width = crop_width + 4;
        let trimmed_height = crop_height + 4;
        let mut trimmed = RgbaImage::new(trimmed_width, trimmed_height);
        for (x, y, pixel) in cropped.enumerate_pixels() {
            trimmed.put_pixel(x + 2, y + 2, *pixel);
        }

        let source = trimmed.clone();
        let alpha = source.pixels().map(|pixel| pixel.0[3]).collect();
        let mask = AlphaMask::try_new(trimmed_width, trimmed_height, alpha)?;
        let softened_alpha = mask.antialiased_threshold(TRIM_ALPHA_THRESHOLD)?;
        for y in 0..trimmed_height {
            for x in 0..trimmed_width {
                let index = y as usize * trimmed_width as usize + x as usize;
                let source_pixel = *source.get_pixel(x, y);
                let is_padding =
                    x < 2 || y < 2 || x + 2 >= trimmed_width || y + 2 >= trimmed_height;
                let result = if is_padding {
                    let alpha = softened_alpha[index];
                    if alpha == 0 {
                        image::Rgba([0, 0, 0, 0])
                    } else if let Some(rgb) = nearest_content_rgb(&source, x, y) {
                        image::Rgba([rgb[0], rgb[1], rgb[2], alpha])
                    } else {
                        image::Rgba([0, 0, 0, 0])
                    }
                } else if source_pixel.0[3] <= TRIM_ALPHA_THRESHOLD {
                    image::Rgba([0, 0, 0, 0])
                } else {
                    image::Rgba([
                        source_pixel.0[0],
                        source_pixel.0[1],
                        source_pixel.0[2],
                        source_pixel.0[3].min(softened_alpha[index]),
                    ])
                };
                trimmed.put_pixel(x, y, result);
            }
        }
        let width_mm = f64::from(trimmed_width) * scale_mm_per_pixel;
        let height_mm = f64::from(trimmed_height) * scale_mm_per_pixel;
        let centered_bounds = BoundsMm {
            left: original_bounds.left + (original_bounds.width - width_mm) / 2.0,
            bottom: original_bounds.bottom + (original_bounds.height - height_mm) / 2.0,
            width: width_mm,
            height: height_mm,
        };
        Ok((trimmed, centered_bounds))
    }

    fn nearest_content_rgb(image: &RgbaImage, x: u32, y: u32) -> Option<[u8; 3]> {
        let mut nearest: Option<(u32, [u8; 3])> = None;
        for candidate_y in y.saturating_sub(1)..=(y + 1).min(image.height() - 1) {
            for candidate_x in x.saturating_sub(1)..=(x + 1).min(image.width() - 1) {
                let pixel = image.get_pixel(candidate_x, candidate_y);
                if pixel.0[3] <= TRIM_ALPHA_THRESHOLD {
                    continue;
                }
                let distance = x.abs_diff(candidate_x).pow(2) + y.abs_diff(candidate_y).pow(2);
                if nearest.is_none_or(|(current, _)| distance < current) {
                    nearest = Some((distance, [pixel.0[0], pixel.0[1], pixel.0[2]]));
                }
            }
        }
        nearest.map(|(_, rgb)| rgb)
    }

    fn import_trimmed_bitmap(
        app: &Dispatch,
        layer: &Dispatch,
        replacement: &BitmapReplacement,
        units: DocumentUnits,
    ) -> Result<Dispatch> {
        import_bitmap_on_layer(layer, &replacement.png_path)?;
        let selection = app.get_dispatch("ActiveSelectionRange")?;
        let imported = selection.get_dispatch_item(1)?;
        imported.put(
            "SizeWidth",
            units.value_from_mm(replacement.bounds.width).into(),
        )?;
        imported.put(
            "SizeHeight",
            units.value_from_mm(replacement.bounds.height).into(),
        )?;
        imported.put("LeftX", units.value_from_mm(replacement.bounds.left).into())?;
        imported.put(
            "BottomY",
            units.value_from_mm(replacement.bounds.bottom).into(),
        )?;
        replacement.source.call("Delete", Vec::new())?;
        Ok(imported)
    }

    fn verify_selection(
        output: &Path,
        selection_report: &Path,
        verification_report: &Path,
    ) -> Result<()> {
        if !output.is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "选择测试 CDR 不存在").into());
        }
        let report: SelectionReport = serde_json::from_slice(&fs::read(selection_report)?)?;
        if report.schema_version != 1 || report.shapes.is_empty() || report.page_index < 1 {
            return Err(io::Error::other("选择处理报告为空或版本不受支持").into());
        }
        let output_hash = sha256(output)?;
        let output = fs::canonicalize(output)?;
        let _apartment = ComApartment::initialize()?;
        let app = coreldraw_application()?;
        let version = app.get_string("Version")?;
        let document = app.call_dispatch("OpenDocument", vec![path_variant(&output)?])?;
        let verification: Result<SelectionVerificationReport> = (|| {
            let units = document_units(&app, &document)?;
            let pages = document.get_dispatch("Pages")?;
            let page = pages.get_dispatch_item(report.page_index)?;
            let layer = find_unique_layer(&page, &report.output_layer)?;
            let shapes = verify_layer_shapes(&layer, &report.shapes, units)?;
            document.call("Close", Vec::new())?;
            Ok(SelectionVerificationReport {
                schema_version: 1,
                coreldraw_version: version,
                output_sha256: output_hash,
                layer_name: report.output_layer,
                shape_count: shapes.len(),
                shapes,
            })
        })();
        if verification.is_err() {
            let _ = document.call("Close", Vec::new());
        }
        let verification: SelectionVerificationReport = verification?;
        if let Some(parent) = verification_report.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            verification_report,
            serde_json::to_vec_pretty(&verification)?,
        )?;
        println!(
            "选择写回验证通过: 图层 {}，{} 个复合曲线",
            verification.layer_name, verification.shape_count
        );
        Ok(())
    }

    fn write(source: &Path, output: &Path, vectors: &Path) -> Result<()> {
        let data = read_and_validate_vectors(source, vectors)?;
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        ensure_distinct_paths(source, output)?;
        fs::copy(source, output)?;
        let output = fs::canonicalize(output)?;

        let _apartment = ComApartment::initialize()?;
        let app = coreldraw_application()?;
        println!("连接 CorelDRAW {}", app.get_string("Version")?);
        let document = app.call_dispatch("OpenDocument", vec![path_variant(&output)?])?;
        let write_result = write_document(&app, &document, &data);
        match write_result {
            Ok(()) => {
                document.call("Save", Vec::new())?;
                document.call("Close", Vec::new())?;
                println!(
                    "已写入 {} 个巡边对象: {}",
                    data.shapes.len(),
                    output.display()
                );
                Ok(())
            }
            Err(error) => {
                let _ = document.call("EndCommandGroup", Vec::new());
                let _ = document.call("Undo", vec![1_i32.into()]);
                let _ = document.call("Close", Vec::new());
                Err(error)
            }
        }
    }

    fn write_document(app: &Dispatch, document: &Dispatch, data: &VectorOutput) -> Result<()> {
        let units = document_units(app, document)?;
        let pages = document.get_dispatch("Pages")?;
        let page = pages.get_dispatch_item(data.page_index)?;
        document.call("BeginCommandGroup", vec!["Rust 巡边与孔位".into()])?;
        let layer = page.call_dispatch("CreateLayer", vec!["RUST_TEST".into()])?;
        write_shapes(app, document, &layer, &data.shapes, units)?;
        document.call("EndCommandGroup", Vec::new())?;
        Ok(())
    }

    fn write_shapes(
        app: &Dispatch,
        document: &Dispatch,
        layer: &Dispatch,
        shapes: &[VectorShape],
        units: DocumentUnits,
    ) -> Result<()> {
        let color = app.call_dispatch(
            "CreateRGBColor",
            vec![236_i32.into(), 42_i32.into(), 144_i32.into()],
        )?;

        for shape_data in shapes {
            validate_shape(shape_data)?;
            let curve = app.call_dispatch("CreateCurve", vec![dispatch_variant(&document.0)])?;
            for polygon in &shape_data.polygons {
                append_ring_with_curve(
                    &curve,
                    &polygon.exterior,
                    polygon.curved_exterior.as_ref(),
                    units,
                )?;
                for (index, interior) in polygon.interiors.iter().enumerate() {
                    let curved = polygon
                        .curved_interiors
                        .as_ref()
                        .and_then(|paths| paths.get(index));
                    append_ring_with_curve(&curve, interior, curved, units)?;
                }
            }
            let shape = layer.call_dispatch("CreateCurve", vec![dispatch_variant(&curve.0)])?;
            shape.put("Name", shape_data.name.as_str().into())?;
            let fill = shape.get_dispatch("Fill")?;
            fill.call("ApplyNoFill", Vec::new())?;
            let outline = shape.get_dispatch("Outline")?;
            outline.put(
                "Width",
                units.value_from_mm(shape_data.outline_width_mm).into(),
            )?;
            outline.put("Color", dispatch_variant(&color.0))?;
            println!("写入 {}", shape_data.source_shape_path);
        }
        Ok(())
    }

    #[cfg(test)]
    fn append_ring(curve: &Dispatch, points: &[PointMm], units: DocumentUnits) -> Result<()> {
        append_ring_with_curve(curve, points, None, units)
    }

    fn append_ring_with_curve(
        curve: &Dispatch,
        points: &[PointMm],
        curved_path: Option<&ClosedPath>,
        units: DocumentUnits,
    ) -> Result<()> {
        if points.len() < 3 {
            return Err(io::Error::other("ring must contain at least three points").into());
        }
        if let Some(path) = curved_path {
            return append_closed_path(curve, path, units);
        }

        let first = &points[0];
        let subpath = curve.call_dispatch(
            "CreateSubPath",
            vec![
                units.value_from_mm(first.x).into(),
                units.value_from_mm(first.y).into(),
            ],
        )?;
        for point in &points[1..] {
            subpath.call(
                "AppendLineSegment",
                vec![
                    units.value_from_mm(point.x).into(),
                    units.value_from_mm(point.y).into(),
                ],
            )?;
        }
        subpath.put("Closed", true.into())?;
        Ok(())
    }

    fn append_closed_path(curve: &Dispatch, path: &ClosedPath, units: DocumentUnits) -> Result<()> {
        let validated = ClosedPath::try_new(path.segments().to_vec())?;
        let first = validated
            .segments()
            .first()
            .ok_or_else(|| io::Error::other("curve path contains no segments"))?
            .start();
        let subpath = curve.call_dispatch(
            "CreateSubPath",
            vec![
                units.value_from_mm(first.x).into(),
                units.value_from_mm(first.y).into(),
            ],
        )?;
        for segment in validated.segments() {
            match segment {
                DomainPathSegment::Line { end, .. } => {
                    subpath.call(
                        "AppendLineSegment",
                        vec![
                            units.value_from_mm(end.x).into(),
                            units.value_from_mm(end.y).into(),
                        ],
                    )?;
                }
                DomainPathSegment::CubicBezier {
                    control_start,
                    control_end,
                    end,
                    ..
                } => {
                    let (starting_length, starting_angle) =
                        control_polar(segment.start(), *control_start);
                    let (ending_length, ending_angle) = control_polar(*end, *control_end);
                    subpath.call(
                        "AppendCurveSegment",
                        vec![
                            units.value_from_mm(end.x).into(),
                            units.value_from_mm(end.y).into(),
                            units.value_from_mm(starting_length).into(),
                            starting_angle.into(),
                            units.value_from_mm(ending_length).into(),
                            ending_angle.into(),
                            false.into(),
                        ],
                    )?;
                }
            }
        }
        subpath.put("Closed", true.into())?;
        Ok(())
    }

    fn control_polar(origin: DomainPointMm, control: DomainPointMm) -> (f64, f64) {
        let dx = control.x - origin.x;
        let dy = control.y - origin.y;
        (dx.hypot(dy), dy.atan2(dx).to_degrees())
    }

    fn validate_selected_bitmap_transform(shape: &Dispatch, index: i32) -> Result<()> {
        let angle = shape.get_f64("RotationAngle")?.rem_euclid(360.0);
        let normalized_angle = angle.min(360.0 - angle);
        let skew = shape.get_f64("AbsoluteSkew")?;
        let horizontal_scale = shape.get_f64("AbsoluteHScale")?;
        let vertical_scale = shape.get_f64("AbsoluteVScale")?;
        if normalized_angle > 1.0e-6 || skew.abs() > 1.0e-6 {
            return Err(io::Error::other(format!(
                "选择中的第 {index} 张位图存在旋转或倾斜；当前版本暂不处理此变换"
            ))
            .into());
        }
        if horizontal_scale <= 0.0 || vertical_scale <= 0.0 {
            return Err(io::Error::other(format!(
                "选择中的第 {index} 张位图存在镜像变换；当前版本暂不处理镜像"
            ))
            .into());
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn export_bitmap_png(
        app: &Dispatch,
        document: &Dispatch,
        original_selection: &Dispatch,
        shape: &Dispatch,
        path: &Path,
        width: i32,
        height: i32,
        resolution_x: i32,
        resolution_y: i32,
    ) -> Result<()> {
        shape.call("CreateSelection", Vec::new())?;
        let export_result: Result<()> = (|| {
            let palette_options = app.call_dispatch("CreateStructPaletteOptions", Vec::new())?;
            let export_area = app.call_dispatch(
                "CreateRect",
                vec![
                    0.0_f64.into(),
                    0.0_f64.into(),
                    0.0_f64.into(),
                    0.0_f64.into(),
                ],
            )?;
            let filter = document.call_dispatch(
                "ExportBitmap",
                vec![
                    path_variant(path)?,
                    802_i32.into(),
                    2_i32.into(),
                    4_i32.into(),
                    width.into(),
                    height.into(),
                    resolution_x.into(),
                    resolution_y.into(),
                    1_i32.into(),
                    false.into(),
                    true.into(),
                    false.into(),
                    false.into(),
                    0_i32.into(),
                    dispatch_variant(&palette_options.0),
                    dispatch_variant(&export_area.0),
                ],
            )?;
            filter.call("Finish", Vec::new())?;
            Ok(())
        })();
        let restore_result = original_selection.call("CreateSelection", Vec::new());
        export_result?;
        restore_result?;
        if !path.is_file() {
            return Err(io::Error::other("Corel 未生成临时 PNG").into());
        }
        Ok(())
    }

    fn unique_layer_name(page: &Dispatch, base: &str) -> Result<String> {
        let layers = page.get_dispatch("Layers")?;
        let count = layers.get_i32("Count")?;
        let mut names = Vec::new();
        for index in 1..=count {
            names.push(layers.get_dispatch_item(index)?.get_string("Name")?);
        }
        if !names.iter().any(|name| name == base) {
            return Ok(base.to_string());
        }
        for suffix in 2..=10_000 {
            let candidate = format!("{base} {suffix}");
            if !names.iter().any(|name| name == &candidate) {
                return Ok(candidate);
            }
        }
        Err(io::Error::other("无法为输出图层生成唯一名称").into())
    }

    fn to_page_polygons(
        geometry: &MultiPolygon<f64>,
        bounds: BoundsMm,
        curved_geometry: Option<&[cdr_raster::CurvedPolygon]>,
    ) -> Result<Vec<VectorPolygon>> {
        if curved_geometry.is_some_and(|curves| curves.len() != geometry.0.len()) {
            return Err(io::Error::other("curved and polygon geometry counts differ").into());
        }
        geometry
            .iter()
            .enumerate()
            .map(|(index, polygon)| {
                let curved = curved_geometry.and_then(|curves| curves.get(index));
                Ok(VectorPolygon {
                    exterior: to_page_ring(polygon.exterior(), bounds, true),
                    interiors: polygon
                        .interiors()
                        .iter()
                        .map(|ring| to_page_ring(ring, bounds, false))
                        .collect(),
                    curved_exterior: curved
                        .map(|curved| to_page_curve_path(&curved.exterior, bounds, true))
                        .transpose()?,
                    curved_interiors: curved
                        .map(|curved| {
                            curved
                                .interiors
                                .iter()
                                .map(|path| to_page_curve_path(path, bounds, false))
                                .collect::<Result<Vec<_>>>()
                        })
                        .transpose()?,
                })
            })
            .collect()
    }

    fn to_page_curve_path(
        path: &ClosedPath,
        bounds: BoundsMm,
        counter_clockwise: bool,
    ) -> Result<ClosedPath> {
        let transformed = path
            .segments()
            .iter()
            .map(|segment| {
                let transform = |point: DomainPointMm| DomainPointMm {
                    x: bounds.left + point.x,
                    y: bounds.bottom + bounds.height - point.y,
                };
                match segment {
                    DomainPathSegment::Line { start, end } => DomainPathSegment::Line {
                        start: transform(*start),
                        end: transform(*end),
                    },
                    DomainPathSegment::CubicBezier {
                        start,
                        control_start,
                        control_end,
                        end,
                    } => DomainPathSegment::CubicBezier {
                        start: transform(*start),
                        control_start: transform(*control_start),
                        control_end: transform(*control_end),
                        end: transform(*end),
                    },
                }
            })
            .collect::<Vec<_>>();
        let transformed = ClosedPath::try_new(transformed)?;
        let anchor_points = transformed
            .segments()
            .iter()
            .map(|segment| {
                let point = segment.start();
                PointMm {
                    x: point.x,
                    y: point.y,
                }
            })
            .collect::<Vec<_>>();
        if (signed_ring_area(&anchor_points) > 0.0) == counter_clockwise {
            Ok(transformed)
        } else {
            reverse_closed_path(&transformed)
        }
    }

    fn reverse_closed_path(path: &ClosedPath) -> Result<ClosedPath> {
        let reversed = path
            .segments()
            .iter()
            .rev()
            .map(|segment| match segment {
                DomainPathSegment::Line { start, end } => DomainPathSegment::Line {
                    start: *end,
                    end: *start,
                },
                DomainPathSegment::CubicBezier {
                    start,
                    control_start,
                    control_end,
                    end,
                } => DomainPathSegment::CubicBezier {
                    start: *end,
                    control_start: *control_end,
                    control_end: *control_start,
                    end: *start,
                },
            })
            .collect();
        Ok(ClosedPath::try_new(reversed)?)
    }

    fn to_page_ring(
        ring: &LineString<f64>,
        bounds: BoundsMm,
        counter_clockwise: bool,
    ) -> Vec<PointMm> {
        let mut points = ring
            .coords()
            .map(|coordinate| PointMm {
                x: bounds.left + coordinate.x,
                y: bounds.bottom + bounds.height - coordinate.y,
            })
            .collect::<Vec<_>>();
        if points.len() > 1 {
            let last = points.len() - 1;
            if (points[0].x - points[last].x).abs() < 1.0e-9
                && (points[0].y - points[last].y).abs() < 1.0e-9
            {
                points.pop();
            }
        }
        let is_counter_clockwise = signed_ring_area(&points) > 0.0;
        if is_counter_clockwise != counter_clockwise {
            points.reverse();
        }
        points
    }

    fn signed_ring_area(points: &[PointMm]) -> f64 {
        if points.len() < 3 {
            return 0.0;
        }
        points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(points.len())
            .map(|(first, second)| first.x * second.y - second.x * first.y)
            .sum::<f64>()
            / 2.0
    }

    fn verify(source: &Path, output: &Path, vectors: &Path, report: &Path) -> Result<()> {
        let data = read_and_validate_vectors(source, vectors)?;
        if !output.is_file() {
            return Err(
                io::Error::new(io::ErrorKind::NotFound, "output CDR does not exist").into(),
            );
        }
        let source_hash = sha256(source)?;
        let output_hash = sha256(output)?;
        let output = fs::canonicalize(output)?;
        let _apartment = ComApartment::initialize()?;
        let app = coreldraw_application()?;
        let version = app.get_string("Version")?;
        let document = app.call_dispatch("OpenDocument", vec![path_variant(&output)?])?;
        let verification = verify_document(&app, &document, &data).and_then(|shapes| {
            document.call("Close", Vec::new())?;
            Ok(VerificationReport {
                schema_version: 1,
                coreldraw_version: version,
                source_sha256: source_hash.clone(),
                output_sha256: output_hash,
                original_unchanged: source_hash.eq_ignore_ascii_case(&data.source_sha256),
                layer_name: "RUST_TEST",
                shape_count: shapes.len().try_into()?,
                shapes,
            })
        });
        if verification.is_err() {
            let _ = document.call("Close", Vec::new());
        }
        let verification = verification?;
        if let Some(parent) = report.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(report, serde_json::to_vec_pretty(&verification)?)?;
        println!("验证通过: {} 个复合曲线", verification.shape_count);
        Ok(())
    }

    fn verify_document(
        app: &Dispatch,
        document: &Dispatch,
        data: &VectorOutput,
    ) -> Result<Vec<VerifiedShape>> {
        let units = document_units(app, document)?;
        let pages = document.get_dispatch("Pages")?;
        let page = pages.get_dispatch_item(data.page_index)?;
        let layer = find_unique_layer(&page, "RUST_TEST")?;
        verify_layer_shapes(&layer, &data.shapes, units)
    }

    fn find_unique_layer(page: &Dispatch, name: &str) -> Result<Dispatch> {
        let layers = page.get_dispatch("Layers")?;
        let layer_count = layers.get_i32("Count")?;
        let mut matching_layer = None;
        for index in 1..=layer_count {
            let layer = layers.get_dispatch_item(index)?;
            if layer.get_string("Name")? == name {
                if matching_layer.is_some() {
                    return Err(io::Error::other(format!("存在多个名为 {name} 的图层")).into());
                }
                matching_layer = Some(layer);
            }
        }
        matching_layer.ok_or_else(|| io::Error::other(format!("未找到图层 {name}")).into())
    }

    fn verify_layer_shapes(
        layer: &Dispatch,
        expected_data: &[VectorShape],
        units: DocumentUnits,
    ) -> Result<Vec<VerifiedShape>> {
        let shapes = layer.get_dispatch("Shapes")?;
        let shape_count = shapes.get_i32("Count")?;
        let mut vector_shapes = Vec::new();
        for index in 1..=shape_count {
            let shape = shapes.get_dispatch_item(index)?;
            if shape.get_i32("Type")? == 3 {
                vector_shapes.push(shape);
            }
        }
        if vector_shapes.len() != expected_data.len() {
            return Err(io::Error::other(format!(
                "expected {} vector curves, found {}",
                expected_data.len(),
                vector_shapes.len()
            ))
            .into());
        }

        let mut verified = Vec::new();
        let mut expected_shapes = expected_data
            .iter()
            .map(|shape| Ok((shape, shape_bounds(shape)?)))
            .collect::<Result<Vec<_>>>()?;
        for shape in vector_shapes {
            let curve = shape.get_dispatch("Curve")?;
            let subpaths = curve.get_dispatch("SubPaths")?;
            let subpath_count = subpaths.get_i32("Count")?;
            let mut closed_subpaths = 0;
            for subpath_index in 1..=subpath_count {
                let subpath = subpaths.get_dispatch_item(subpath_index)?;
                if bool::try_from(&subpath.get("Closed")?)? {
                    closed_subpaths += 1;
                }
            }
            let fill = shape.get_dispatch("Fill")?;
            let outline = shape.get_dispatch("Outline")?;
            let color = outline.get_dispatch("Color")?;
            let bounds_mm = BoundsMm {
                left: units.to_mm(shape.get_f64("LeftX")?),
                bottom: units.to_mm(shape.get_f64("BottomY")?),
                width: units.to_mm(shape.get_f64("SizeWidth")?),
                height: units.to_mm(shape.get_f64("SizeHeight")?),
            };
            let expected_index = expected_shapes
                .iter()
                .enumerate()
                .min_by(|(_, (_, first)), (_, (_, second))| {
                    bounds_distance(bounds_mm, *first)
                        .total_cmp(&bounds_distance(bounds_mm, *second))
                })
                .map(|(index, _)| index)
                .ok_or_else(|| io::Error::other("no unmatched vector shape remains"))?;
            let (expected, expected_bounds) = expected_shapes.remove(expected_index);
            let item = VerifiedShape {
                source_shape_path: expected.source_shape_path.clone(),
                name: shape.get_string("Name")?,
                shape_type: shape.get_i32("Type")?,
                subpath_count,
                closed_subpaths,
                fill_type: fill.get_i32("Type")?,
                outline_type: outline.get_i32("Type")?,
                outline_width_mm: units.to_mm(outline.get_f64("Width")?),
                outline_rgb: [
                    color.get_i32("RGBRed")?,
                    color.get_i32("RGBGreen")?,
                    color.get_i32("RGBBlue")?,
                ],
                bounds_mm,
            };
            let expected_subpaths = expected
                .polygons
                .iter()
                .map(|polygon| 1 + polygon.interiors.len())
                .sum::<usize>();
            validate_verified_shape(&item, expected_subpaths.try_into()?, expected_bounds)?;
            verified.push(item);
        }
        Ok(verified)
    }

    fn validate_shape(shape: &VectorShape) -> Result<()> {
        if !is_supported_shape_name(&shape.name)
            || !shape.outline_hex.eq_ignore_ascii_case("#EC2A90")
            || (shape.outline_width_mm - 0.076).abs() > 1.0e-9
            || shape.polygons.is_empty()
        {
            return Err(
                io::Error::other("vector shape contains unsupported styling or geometry").into(),
            );
        }
        Ok(())
    }

    fn is_supported_shape_name(name: &str) -> bool {
        if name == "h_xbxb" {
            return true;
        }
        let Some((prefix, static_id)) = name.rsplit_once('_') else {
            return false;
        };
        matches!(
            prefix,
            "CDR_RUST_OUTLINE" | "CDR_RUST_HOLE" | "CDR_RUST_MERGED" | "CDR_RUST_TRIMMED_OUTLINE"
        ) && static_id.parse::<i32>().is_ok()
    }

    fn validate_verified_shape(
        shape: &VerifiedShape,
        expected_subpaths: i32,
        expected_bounds: BoundsMm,
    ) -> Result<()> {
        if !is_supported_shape_name(&shape.name)
            || shape.shape_type != 3
            || shape.subpath_count != expected_subpaths
            || shape.closed_subpaths != expected_subpaths
            || shape.fill_type != 0
            || shape.outline_type == 0
            || (shape.outline_width_mm - 0.076).abs() > 0.002
            || shape.outline_rgb != [236, 42, 144]
            || bounds_distance(shape.bounds_mm, expected_bounds) > 0.2
        {
            return Err(io::Error::other(format!("unexpected Corel shape: {shape:?}")).into());
        }
        Ok(())
    }

    fn document_units(app: &Dispatch, document: &Dispatch) -> Result<DocumentUnits> {
        let unit = document.get_i32("Unit")?;
        let converted = app.call(
            "ConvertUnits",
            vec![1.0_f64.into(), unit.into(), MILLIMETER_UNIT.into()],
        )?;
        let millimeters_per_unit = f64::try_from(&converted)?;
        if !millimeters_per_unit.is_finite() || millimeters_per_unit <= 0.0 {
            return Err(io::Error::other("Corel returned an invalid document unit scale").into());
        }
        Ok(DocumentUnits {
            millimeters_per_unit,
        })
    }

    fn shape_bounds(shape: &VectorShape) -> Result<BoundsMm> {
        let mut points = shape
            .polygons
            .iter()
            .flat_map(|polygon| polygon.exterior.iter());
        let first = points
            .next()
            .ok_or_else(|| io::Error::other("vector shape has no exterior points"))?;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
        for point in points {
            min_x = min_x.min(point.x);
            max_x = max_x.max(point.x);
            min_y = min_y.min(point.y);
            max_y = max_y.max(point.y);
        }
        Ok(BoundsMm {
            left: min_x,
            bottom: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        })
    }

    fn bounds_distance(first: BoundsMm, second: BoundsMm) -> f64 {
        (first.left - second.left)
            .abs()
            .max((first.bottom - second.bottom).abs())
            .max((first.width - second.width).abs())
            .max((first.height - second.height).abs())
    }

    fn read_and_validate_vectors(source: &Path, vectors: &Path) -> Result<VectorOutput> {
        let data: VectorOutput = serde_json::from_slice(&fs::read(vectors)?)?;
        if data.schema_version != 1 || data.page_index < 1 || data.shapes.is_empty() {
            return Err(io::Error::other("unsupported or empty vector output").into());
        }
        let source_hash = sha256(source)?;
        if !source_hash.eq_ignore_ascii_case(&data.source_sha256) {
            return Err(io::Error::other("source CDR hash does not match vector output").into());
        }
        Ok(data)
    }

    fn ensure_distinct_paths(source: &Path, output: &Path) -> Result<()> {
        let source = fs::canonicalize(source)?;
        let output = if output.exists() {
            fs::canonicalize(output)?
        } else {
            let parent = output.parent().unwrap_or_else(|| Path::new("."));
            fs::canonicalize(parent)?.join(
                output
                    .file_name()
                    .ok_or_else(|| io::Error::other("output path has no file name"))?,
            )
        };
        if source == output {
            return Err(io::Error::other("refusing to overwrite the source CDR").into());
        }
        Ok(())
    }

    fn coreldraw_application() -> Result<Dispatch> {
        let prog_id = wide_null(CORELDRAW_PROG_ID);
        let class_id = unsafe { CLSIDFromProgID(PCWSTR(prog_id.as_ptr()))? };
        let dispatch: IDispatch =
            unsafe { CoCreateInstance(&class_id, None, CLSCTX_LOCAL_SERVER)? };
        Ok(Dispatch(dispatch))
    }

    fn variant_dispatch(value: VARIANT) -> Result<Dispatch> {
        let raw = value.as_raw();
        let variant = unsafe { raw.Anonymous.Anonymous };
        let dispatch = if variant.vt == VT_DISPATCH.0 {
            let pointer = unsafe { variant.Anonymous.pdispVal };
            if pointer.is_null() {
                return Err(io::Error::other("COM returned a null IDispatch").into());
            }
            let borrowed = unsafe { IDispatch::from_raw(pointer) };
            let cloned = borrowed.clone();
            std::mem::forget(borrowed);
            cloned
        } else if variant.vt == VT_UNKNOWN.0 {
            let pointer = unsafe { variant.Anonymous.punkVal };
            if pointer.is_null() {
                return Err(io::Error::other("COM returned a null IUnknown").into());
            }
            let borrowed = unsafe { IUnknown::from_raw(pointer) };
            let dispatch = borrowed.cast::<IDispatch>()?;
            std::mem::forget(borrowed);
            dispatch
        } else {
            return Err(io::Error::other(format!(
                "COM returned VARIANT type {}, expected dispatch",
                variant.vt
            ))
            .into());
        };
        Ok(Dispatch(dispatch))
    }

    fn dispatch_variant(value: &IDispatch) -> VARIANT {
        value.clone().into()
    }

    fn path_variant(path: &Path) -> Result<VARIANT> {
        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        Ok(BSTR::from_wide(&wide)?.into())
    }

    fn import_bitmap_on_layer(layer: &Dispatch, path: &Path) -> Result<()> {
        type ImportMethod = unsafe extern "system" fn(
            *mut std::ffi::c_void,
            *const u16,
            i32,
            *mut std::ffi::c_void,
        ) -> windows::core::HRESULT;

        let layer_interface = layer.0.cast::<CorelLayerInterface>()?;
        let path_wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        let path_bstr = BSTR::from_wide(&path_wide)?;
        let vtable =
            unsafe { *(layer_interface.as_raw() as *const *const *const std::ffi::c_void) };
        let method_pointer = unsafe { *vtable.add(CORELDRAW_LAYER_IMPORT_SLOT) };
        let import: ImportMethod = unsafe { std::mem::transmute(method_pointer) };
        unsafe {
            import(
                layer_interface.as_raw(),
                path_bstr.as_ptr(),
                0,
                ptr::null_mut(),
            )
        }
        .ok()
        .map_err(|error| com_error("Layer.Import", error))?;
        Ok(())
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn sha256(path: &Path) -> Result<String> {
        let digest = Sha256::digest(fs::read(path)?);
        Ok(format!("{digest:X}"))
    }

    fn com_error(member: &str, error: windows::core::Error) -> io::Error {
        io::Error::other(format!("COM {member} failed: {error}"))
    }

    #[cfg(test)]
    mod tests {
        use std::fs;

        use super::{
            BoundsMm, ProcessingSettings, SelectionOperation, VectorPolygon, VectorShape,
            VerifiedShape, generated_shape_name, parse_generated_outline_id, validate_shape,
            validate_verified_shape,
        };

        #[test]
        fn generated_outline_selection_resolves_its_bitmap_id() {
            assert_eq!(parse_generated_outline_id("CDR_RUST_OUTLINE_42"), Some(42));
            assert_eq!(
                parse_generated_outline_id("CDR_RUST_TRIMMED_OUTLINE_42"),
                Some(42)
            );
            assert_eq!(parse_generated_outline_id("CDR_RUST_HOLE_42"), None);
        }

        #[test]
        fn inspects_corel_svg_with_external_doctype() {
            let svg = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;

            assert!(super::inspect_svg_images(svg).unwrap().is_empty());
        }

        #[test]
        fn native_image_coordinates_are_preserved_without_a_transform() {
            let tag = r#"<image x="-713.8" y="-354.9" width="1871.13" height="2421.46" xlink:href="data:image/png;base64,AAAA"/>"#;

            let placed = super::place_bitmap_svg_tag(tag, super::identity_matrix()).unwrap();

            assert_eq!(placed, tag);
            assert!(!placed.contains("transform="));
        }

        #[test]
        fn ordinary_native_images_are_left_untouched_when_not_replaced() {
            let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image x="-10" y="-20" width="100" height="80" xlink:href="data:image/png;base64,AAAA"/></svg>"#;
            let images = super::inspect_svg_images(svg).unwrap();
            assert!(images[0].embedded_data);

            let output = super::replace_svg_images_with_pngs(svg, &images, &[0], &[None]).unwrap();

            assert_eq!(output, svg);
        }

        #[test]
        fn only_requested_bitmap_images_are_replaced() {
            let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image x="0" y="0" width="10" height="10" xlink:href="data:image/png;base64,OLD1"/><image x="20" y="0" width="10" height="10" xlink:href="data:image/png;base64,OLD2"/></svg>"#;
            let images = super::inspect_svg_images(svg).unwrap();

            let output = super::replace_svg_images_with_pngs(
                svg,
                &images,
                &[0, 1],
                &[Some(vec![1, 2, 3]), None],
            )
            .unwrap();

            assert!(output.contains("data:image/png;base64,AQID"));
            assert!(output.contains("data:image/png;base64,OLD2"));
            assert!(!output.contains("data:image/png;base64,OLD1"));
        }

        #[test]
        fn vector_output_roundtrips_bezier_paths_and_accepts_legacy_polygons() {
            use cdr_domain::{ClosedPath, PathSegment, PointMm as DomainPointMm};

            let origin = DomainPointMm::new(2.0, 3.0);
            let path = ClosedPath::try_new(vec![PathSegment::CubicBezier {
                start: origin,
                control_start: DomainPointMm::new(2.0, 4.0),
                control_end: DomainPointMm::new(3.0, 3.0),
                end: origin,
            }])
            .unwrap();
            let polygon = VectorPolygon {
                exterior: vec![
                    super::PointMm { x: 1.0, y: 1.0 },
                    super::PointMm { x: 2.0, y: 1.0 },
                    super::PointMm { x: 1.0, y: 2.0 },
                ],
                interiors: Vec::new(),
                curved_exterior: Some(path.clone()),
                curved_interiors: Some(Vec::new()),
            };

            let encoded = serde_json::to_vec(&polygon).unwrap();
            let decoded: VectorPolygon = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.curved_exterior, Some(path));
            assert_eq!(decoded.curved_interiors, Some(Vec::new()));

            let legacy = br#"{"exterior":[{"x":0.0,"y":0.0},{"x":1.0,"y":0.0},{"x":0.0,"y":1.0}],"interiors":[]}"#;
            let decoded: VectorPolygon = serde_json::from_slice(legacy).unwrap();
            assert!(decoded.curved_exterior.is_none());
            assert!(decoded.curved_interiors.is_none());
        }

        #[test]
        fn generated_outline_name_passes_the_writer_validation() {
            let mut shape = VectorShape {
                source_shape_path: "selection.1.id-42".to_owned(),
                name: generated_shape_name(SelectionOperation::Outline, 42),
                outline_hex: "#EC2A90".to_owned(),
                outline_width_mm: 0.076,
                polygons: vec![VectorPolygon {
                    exterior: Vec::new(),
                    interiors: Vec::new(),
                    curved_exterior: None,
                    curved_interiors: None,
                }],
            };

            let generated_name = shape.name.clone();
            shape.name = "h_xbxb".to_owned();
            assert!(
                validate_shape(&shape).is_ok(),
                "legacy test style must remain accepted"
            );
            shape.name = generated_name;
            assert!(
                validate_shape(&shape).is_ok(),
                "production-generated outline was rejected by the Corel writer"
            );
        }

        #[test]
        fn generated_outline_name_passes_corel_readback_validation() {
            let bounds = BoundsMm {
                left: 125.327,
                bottom: 230.3539,
                width: 40.833,
                height: 42.9448,
            };
            let verified = VerifiedShape {
                source_shape_path: "selection.3.id-11".to_owned(),
                name: "CDR_RUST_OUTLINE_11".to_owned(),
                shape_type: 3,
                subpath_count: 1,
                closed_subpaths: 1,
                fill_type: 0,
                outline_type: 1,
                outline_width_mm: 0.076,
                outline_rgb: [236, 42, 144],
                bounds_mm: bounds,
            };

            assert!(validate_verified_shape(&verified, 1, bounds).is_ok());
        }

        #[test]
        fn transparent_trim_removes_low_alpha_fringe_pixels() {
            let mut image = image::RgbaImage::new(5, 5);
            for y in 1..=3 {
                for x in 1..=3 {
                    image.put_pixel(x, y, image::Rgba([220, 120, 80, 255]));
                }
            }
            image.put_pixel(0, 2, image::Rgba([20, 220, 80, 32]));
            let alpha = image.pixels().map(|pixel| pixel.0[3]).collect();
            let mask = super::AlphaMask::try_new(image.width(), image.height(), alpha).unwrap();
            let (trimmed, _) = super::trim_rgba_image(
                &image,
                &mask,
                super::BoundsMm {
                    left: 0.0,
                    bottom: 0.0,
                    width: 50.0,
                    height: 50.0,
                },
            )
            .unwrap();

            assert!((180..=205).contains(&trimmed.get_pixel(2, 3).0[3]));
            assert!((50..=80).contains(&trimmed.get_pixel(1, 3).0[3]));
            assert_eq!(trimmed.get_pixel(1, 3).0[..3], [220, 120, 80]);
            assert_eq!(trimmed.get_pixel(3, 3).0[3], 255);
            assert_eq!(trimmed.get_pixel(5, 3).0[3], 64);
            assert_eq!(trimmed.get_pixel(0, 3).0[3], 0);
            assert_eq!(trimmed.get_pixel(6, 3).0[3], 0);
        }

        #[test]
        #[ignore = "requires CorelDRAW 2020; creates and closes a temporary unsaved document"]
        fn temporary_document_imports_a_png_and_creates_one_shape() {
            let result = (|| -> super::Result<()> {
                let _apartment = super::ComApartment::initialize()?;
                let app = super::coreldraw_application()?;
                let document = app.call_dispatch("CreateDocument", Vec::new())?;
                let import_result: super::Result<()> = (|| -> super::Result<()> {
                    let temp = tempfile::tempdir()?;
                    let png_path = temp.path().join("cdr-rust-import-probe.png");
                    image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]))
                        .save(&png_path)?;
                    let page = document.get_dispatch("ActivePage")?;
                    let layer = page.get_dispatch("ActiveLayer")?;
                    super::import_bitmap_on_layer(&layer, &png_path)?;
                    let shape_count = layer.get_dispatch("Shapes")?.get_i32("Count")?;
                    if shape_count != 1 {
                        return Err(std::io::Error::other(format!(
                            "expected one imported PNG, found {shape_count}"
                        ))
                        .into());
                    }
                    Ok(())
                })();
                let _ = document.put("Dirty", false.into());
                let close_result = document.call("Close", Vec::new());
                import_result?;
                close_result?;
                Ok(())
            })();

            assert!(
                result.is_ok(),
                "CorelDRAW 2020 PNG import failed in a temporary document: {}",
                result.unwrap_err()
            );
        }

        #[test]
        #[ignore = "requires CorelDRAW 2020; creates and closes a temporary unsaved document"]
        fn temporary_document_imports_hole_rings_as_four_bezier_segments() {
            let result = (|| -> super::Result<()> {
                use geo::{Rect, coord};

                let _apartment = super::ComApartment::initialize()?;
                let app = super::coreldraw_application()?;
                let document = app.call_dispatch("CreateDocument", Vec::new())?;
                let import_result: super::Result<()> = (|| -> super::Result<()> {
                    let outline = geo::MultiPolygon::from(Rect::new(
                        coord! { x: 0.0, y: 0.0 },
                        coord! { x: 10.0, y: 10.0 },
                    ));
                    let hole_ring = super::add_top_center_hole_ring(
                        &outline,
                        super::HolePlacementSettings {
                            diameter_mm: 3.0,
                            edge_clearance_mm: 2.0,
                            smoothing_mm: 0.02,
                        },
                    )?;
                    let units = super::document_units(&app, &document)?;
                    let layer = document
                        .get_dispatch("ActivePage")?
                        .get_dispatch("ActiveLayer")?;
                    let bounds = super::BoundsMm {
                        left: 0.0,
                        bottom: 0.0,
                        width: 10.0,
                        height: 10.0,
                    };
                    let shape_data = super::VectorShape {
                        source_shape_path: "selection.1.id-101".to_owned(),
                        name: "CDR_RUST_HOLE_101".to_owned(),
                        outline_hex: "#EC2A90".to_owned(),
                        outline_width_mm: 0.076,
                        polygons: super::to_page_polygons(
                            &hole_ring.geometry,
                            bounds,
                            hole_ring.curved_geometry.as_deref(),
                        )?,
                    };
                    super::write_shapes(&app, &document, &layer, &[shape_data], units)?;
                    let shape = layer.get_dispatch("Shapes")?.get_dispatch_item(1)?;
                    let subpaths = shape.get_dispatch("Curve")?.get_dispatch("SubPaths")?;
                    if subpaths.get_i32("Count")? != 2 {
                        return Err(std::io::Error::other(
                            "CorelDRAW did not import the outer and center-hole paths",
                        )
                        .into());
                    }
                    for index in 1..=subpaths.get_i32("Count")? {
                        let segments = subpaths
                            .get_dispatch_item(index)?
                            .get_dispatch("Segments")?;
                        if segments.get_i32("Count")? != 4 {
                            return Err(std::io::Error::other(format!(
                                "expected four Bezier arcs, found {} segments",
                                segments.get_i32("Count")?
                            ))
                            .into());
                        }
                        for segment_index in 1..=segments.get_i32("Count")? {
                            let segment = segments.get_dispatch_item(segment_index)?;
                            if segment.get_i32("Type")? != 1 {
                                return Err(std::io::Error::other(
                                    "CorelDRAW converted a Bezier arc to a line segment",
                                )
                                .into());
                            }
                        }
                    }
                    Ok(())
                })();
                let _ = document.put("Dirty", false.into());
                let close_result = document.call("Close", Vec::new());
                import_result?;
                close_result?;
                Ok(())
            })();

            assert!(
                result.is_ok(),
                "CorelDRAW 2020 did not preserve the hole rings as Bezier segments: {}",
                result.unwrap_err()
            );
        }

        #[test]
        #[ignore = "requires CorelDRAW 2020; creates and closes a temporary unsaved document"]
        fn temporary_document_weld_preserves_the_center_hole_path() {
            let result = (|| -> super::Result<()> {
                let _apartment = super::ComApartment::initialize()?;
                let app = super::coreldraw_application()?;
                let document = app.call_dispatch("CreateDocument", Vec::new())?;
                let merge_result: super::Result<(bool, bool, Vec<i32>, f64, i32, i32)> =
                    (|| -> super::Result<(bool, bool, Vec<i32>, f64, i32, i32)> {
                        use geo::{Rect, coord};

                        let page = document.get_dispatch("ActivePage")?;
                        let layer = page.get_dispatch("ActiveLayer")?;
                        let units = super::document_units(&app, &document)?;
                        let outline = create_test_curve(
                            &app,
                            &document,
                            &layer,
                            "CDR_RUST_OUTLINE_101",
                            &[(0.0, 0.0), (40.0, 0.0), (40.0, 40.0), (0.0, 40.0)],
                            units,
                        )?;

                        let outline_geometry = geo::MultiPolygon::from(Rect::new(
                            coord! { x: 0.0, y: 0.0 },
                            coord! { x: 40.0, y: 40.0 },
                        ));
                        let hole_ring = super::add_top_center_hole_ring(
                            &outline_geometry,
                            super::HolePlacementSettings {
                                diameter_mm: 3.5,
                                edge_clearance_mm: 2.0,
                                smoothing_mm: 0.02,
                            },
                        )?;
                        let curved_ring = &hole_ring.curved_geometry.as_ref().unwrap()[0];
                        let hole_curve = app.call_dispatch(
                            "CreateCurve",
                            vec![super::dispatch_variant(&document.0)],
                        )?;
                        super::append_closed_path(&hole_curve, &curved_ring.exterior, units)?;
                        super::append_closed_path(&hole_curve, &curved_ring.interiors[0], units)?;
                        let interior_line = hole_curve.call_dispatch(
                            "CreateSubPath",
                            vec![
                                units.value_from_mm(19.0).into(),
                                units.value_from_mm(39.5).into(),
                            ],
                        )?;
                        interior_line.call(
                            "AppendLineSegment",
                            vec![
                                units.value_from_mm(21.0).into(),
                                units.value_from_mm(40.5).into(),
                            ],
                        )?;
                        let hole = layer.call_dispatch(
                            "CreateCurve",
                            vec![super::dispatch_variant(&hole_curve.0)],
                        )?;
                        hole.put("Name", "CDR_RUST_HOLE_101".into())?;
                        hole.get_dispatch("Fill")?.call("ApplyNoFill", Vec::new())?;

                        outline.call("AddToSelection", Vec::new())?;
                        hole.call("AddToSelection", Vec::new())?;
                        let selection = app.get_dispatch("ActiveSelectionRange")?;
                        let outcome = super::merge_selected_holes(
                            &document,
                            &selection,
                            selection.get_i32("Count")?,
                            None,
                            &mut |_, _| {},
                        )?;
                        assert_eq!(outcome.processed_count, 1);

                        let shapes = layer.get_dispatch("Shapes")?;
                        let mut merged = None;
                        for index in 1..=shapes.get_i32("Count")? {
                            let shape = shapes.get_dispatch_item(index)?;
                            if shape.get_string("Name")?.starts_with("CDR_RUST_MERGED_") {
                                merged = Some(shape);
                                break;
                            }
                        }
                        let merged = merged.ok_or_else(|| {
                            std::io::Error::other("CorelDRAW did not create the merged outline")
                        })?;
                        let subpaths = merged.get_dispatch("Curve")?.get_dispatch("SubPaths")?;
                        let mut found_center_circle = false;
                        let mut found_outer_curve = false;
                        let mut center_circle_segment_types = Vec::new();
                        let mut merged_subpath_count = 0;
                        let mut open_subpath_count = 0;
                        for index in 1..=subpaths.get_i32("Count")? {
                            let subpath = subpaths.get_dispatch_item(index)?;
                            let is_closed = bool::try_from(&subpath.get("Closed")?)?;
                            merged_subpath_count += 1;
                            if !is_closed {
                                open_subpath_count += 1;
                            }
                            let width_mm = units.to_mm(subpath.get_f64("SizeWidth")?);
                            let height_mm = units.to_mm(subpath.get_f64("SizeHeight")?);
                            if is_closed
                                && (width_mm - 40.0).abs() < 0.2
                                && (height_mm - 43.75).abs() < 0.3
                            {
                                let segments = subpath.get_dispatch("Segments")?;
                                for segment_index in 1..=segments.get_i32("Count")? {
                                    found_outer_curve |= segments
                                        .get_dispatch_item(segment_index)?
                                        .get_i32("Type")?
                                        == 1;
                                }
                            }
                            if is_closed
                                && (width_mm - 3.5).abs() < 0.2
                                && (height_mm - 3.5).abs() < 0.2
                            {
                                found_center_circle = true;
                                let segments = subpath.get_dispatch("Segments")?;
                                center_circle_segment_types
                                    .reserve(segments.get_i32("Count")? as usize);
                                for segment_index in 1..=segments.get_i32("Count")? {
                                    center_circle_segment_types.push(
                                        segments
                                            .get_dispatch_item(segment_index)?
                                            .get_i32("Type")?,
                                    );
                                }
                                break;
                            }
                        }
                        let merged_height_mm = units.to_mm(merged.get_f64("SizeHeight")?);
                        Ok((
                            found_center_circle,
                            found_outer_curve,
                            center_circle_segment_types,
                            merged_height_mm,
                            merged_subpath_count,
                            open_subpath_count,
                        ))
                    })();
                let _ = document.put("Dirty", false.into());
                let close_result = document.call("Close", Vec::new());
                let (
                    found_center_circle,
                    found_outer_curve,
                    center_circle_segment_types,
                    merged_height_mm,
                    merged_subpath_count,
                    open_subpath_count,
                ) = merge_result?;
                close_result?;
                if !found_center_circle {
                    return Err(std::io::Error::other(
                        "the merged contour did not retain the 3.5 mm inner cutout as a closed path",
                    )
                    .into());
                }
                if !found_outer_curve {
                    return Err(std::io::Error::other(
                        "the welded support-circle arc was flattened into line segments",
                    )
                    .into());
                }
                if center_circle_segment_types.is_empty()
                    || center_circle_segment_types
                        .iter()
                        .any(|type_id| *type_id != 1)
                {
                    return Err(std::io::Error::other(
                        format!("the welded center hole was not preserved as Bezier segments: {center_circle_segment_types:?}"),
                    )
                    .into());
                }
                if (merged_height_mm - 43.75).abs() > 0.3 {
                    return Err(std::io::Error::other(format!(
                        "the merged outer ring was not welded onto the outline (height {merged_height_mm:.3} mm)"
                    ))
                    .into());
                }
                if merged_subpath_count != 2 || open_subpath_count != 0 {
                    return Err(std::io::Error::other(format!(
                        "the merged result contains unwanted paths: total={merged_subpath_count}, open={open_subpath_count}; expected only the outer contour and inner circle"
                    ))
                    .into());
                }
                Ok(())
            })();

            assert!(
                result.is_ok(),
                "CorelDRAW hole merge failed in a temporary document: {}",
                result.unwrap_err()
            );
        }

        #[test]
        #[ignore = "requires CorelDRAW 2020; creates and closes a temporary unsaved document"]
        fn temporary_document_exports_bitmap_selection_and_page_as_svg() {
            let result = (|| -> super::Result<()> {
                let _apartment = super::ComApartment::initialize()?;
                let app = super::coreldraw_application()?;
                let document = app.call_dispatch("CreateDocument", Vec::new())?;
                let export_result: super::Result<()> = (|| -> super::Result<()> {
                    let temporary = tempfile::tempdir()?;
                    let page = document.get_dispatch("ActivePage")?;
                    let layer = page.get_dispatch("ActiveLayer")?;
                    let bitmap_path = temporary.path().join("svg-probe.png");
                    let mut bitmap_image = image::RgbaImage::new(8, 8);
                    for y in 1..=6 {
                        for x in 1..=6 {
                            bitmap_image.put_pixel(x, y, image::Rgba([30, 140, 220, 255]));
                        }
                    }
                    bitmap_image.save(&bitmap_path)?;
                    super::import_bitmap_on_layer(&layer, &bitmap_path)?;
                    let bitmap = layer.get_dispatch("Shapes")?.get_dispatch_item(1)?;
                    let units = super::document_units(&app, &document)?;
                    let shape = create_test_curve(
                        &app,
                        &document,
                        &layer,
                        "CDR_RUST_SVG_EXPORT_PROBE",
                        &[(0.0, 0.0), (20.0, 0.0), (20.0, 15.0), (0.0, 15.0)],
                        units,
                    )?;
                    bitmap.call("AddToSelection", Vec::new())?;
                    shape.call("AddToSelection", Vec::new())?;

                    for (selected, name) in [(true, "selection.svg"), (false, "page.svg")] {
                        let path = temporary.path().join(name);
                        super::export_document_svg(&document, &app, &path, selected)?;
                        let svg = fs::read_to_string(&path)?;
                        if !svg.contains("<svg") {
                            return Err(std::io::Error::other(format!(
                                "CorelDRAW SVG export produced no SVG root for {name}"
                            ))
                            .into());
                        }
                        if !svg.contains("<image") {
                            return Err(std::io::Error::other(format!(
                                "CorelDRAW SVG export lost the bitmap for {name}"
                            ))
                            .into());
                        }
                    }
                    Ok(())
                })();
                let _ = document.put("Dirty", false.into());
                let close_result = document.call("Close", Vec::new());
                export_result?;
                close_result?;
                Ok(())
            })();

            assert!(
                result.is_ok(),
                "CorelDRAW selection/page SVG export failed in a temporary document: {}",
                result.unwrap_err()
            );
        }

        #[test]
        #[ignore = "requires CorelDRAW 2020; creates and closes a temporary unsaved document"]
        fn temporary_document_trims_to_bitmap_and_vector_contour_with_a_soft_edge() {
            let result = (|| -> super::Result<bool> {
                let _apartment = super::ComApartment::initialize()?;
                let app = super::coreldraw_application()?;
                let document = app.call_dispatch("CreateDocument", Vec::new())?;
                let trim_result: super::Result<bool> = (|| -> super::Result<bool> {
                    let temporary = tempfile::tempdir()?;
                    let input_path = temporary.path().join("fringed-bitmap.png");
                    let mut image = image::RgbaImage::new(20, 20);
                    for y in 3..=16 {
                        for x in 3..=16 {
                            image.put_pixel(x, y, image::Rgba([240, 60, 30, 255]));
                        }
                    }
                    image.put_pixel(2, 8, image::Rgba([12, 240, 20, 48]));
                    image.save(&input_path)?;

                    let page = document.get_dispatch("ActivePage")?;
                    let layer = page.get_dispatch("ActiveLayer")?;
                    super::import_bitmap_on_layer(&layer, &input_path)?;
                    let imported = layer.get_dispatch("Shapes")?.get_dispatch_item(1)?;
                    imported.call("AddToSelection", Vec::new())?;
                    let outcome = super::process_selection_with_progress(
                        super::ProcessingSettings::default(),
                        super::SelectionOperation::TrimTransparent,
                        None,
                        |_, _| {},
                    )?;
                    if outcome.processed_count != 1 {
                        return Err(std::io::Error::other(format!(
                            "expected one trimmed bitmap, processed {}",
                            outcome.processed_count
                        ))
                        .into());
                    }

                    let layers = page.get_dispatch("Layers")?;
                    let mut page_shapes = Vec::new();
                    for layer_index in 1..=layers.get_i32("Count")? {
                        let layer = layers.get_dispatch_item(layer_index)?;
                        let shapes = layer.get_dispatch("Shapes")?;
                        for shape_index in 1..=shapes.get_i32("Count")? {
                            page_shapes.push(shapes.get_dispatch_item(shape_index)?);
                        }
                    }
                    let composite_index = page_shapes.iter().position(|shape| {
                        shape.get_i32("Type").ok() == Some(7)
                            && shape
                                .get_string("Name")
                                .is_ok_and(|name| name.starts_with("CDR_RUST_TRIMMED_OUTLINE_"))
                    });
                    if page_shapes.len() != 1 || composite_index.is_none() {
                        let shapes = page_shapes
                            .iter()
                            .map(|shape| {
                                format!(
                                    "type={} name={}",
                                    shape.get_i32("Type").unwrap_or(-1),
                                    shape.get_string("Name").unwrap_or_default()
                                )
                            })
                            .collect::<Vec<_>>();
                        return Err(std::io::Error::other(format!(
                            "transparent cleanup must leave one composite group, found {} page shapes: {}",
                            page_shapes.len(), shapes.join("; "),
                        ))
                        .into());
                    }

                    let composite = &page_shapes[composite_index.unwrap()];
                    let children = composite.get_dispatch("Shapes")?;
                    if children.get_i32("Count")? != 2 {
                        return Err(std::io::Error::other(format!(
                            "transparent cleanup group must contain a bitmap and vector contour, found {} children",
                            children.get_i32("Count")?
                        ))
                        .into());
                    }
                    let mut bitmap_shape = None;
                    let mut vector_outline = None;
                    for index in 1..=children.get_i32("Count")? {
                        let child = children.get_dispatch_item(index)?;
                        match child.get_i32("Type")? {
                            5 => bitmap_shape = Some(child),
                            3 if child
                                .get_string("Name")?
                                .starts_with("CDR_RUST_TRIMMED_OUTLINE_") =>
                            {
                                vector_outline = Some(child)
                            }
                            _ => {}
                        }
                    }
                    let bitmap_shape = bitmap_shape.ok_or_else(|| {
                        std::io::Error::other("composite trim result has no bitmap child")
                    })?;
                    let vector_outline = vector_outline.ok_or_else(|| {
                        std::io::Error::other("composite trim result has no vector contour child")
                    })?;
                    let bitmap_static_id = bitmap_shape.get_i32("StaticID")?;
                    if composite.get_string("Name")?
                        != super::generated_shape_name(
                            super::SelectionOperation::TrimTransparent,
                            bitmap_static_id,
                        )
                        || vector_outline.get_string("Name")?
                            != super::generated_shape_name(
                                super::SelectionOperation::TrimTransparent,
                                bitmap_static_id,
                            )
                    {
                        return Err(std::io::Error::other(
                            "trim contour is not linked to the replacement bitmap",
                        )
                        .into());
                    }
                    let subpaths = vector_outline
                        .get_dispatch("Curve")?
                        .get_dispatch("SubPaths")?;
                    if subpaths.get_i32("Count")? == 0 {
                        return Err(std::io::Error::other(
                            "transparent cleanup created an empty vector contour",
                        )
                        .into());
                    }
                    for index in 1..=subpaths.get_i32("Count")? {
                        if !bool::try_from(&subpaths.get_dispatch_item(index)?.get("Closed")?)? {
                            return Err(std::io::Error::other(
                                "transparent cleanup created an open vector contour",
                            )
                            .into());
                        }
                    }
                    let bitmap = bitmap_shape.get_dispatch("Bitmap")?;
                    let output_path = temporary.path().join("trimmed-roundtrip.png");
                    let selection = app.get_dispatch("ActiveSelectionRange")?;
                    super::export_bitmap_png(
                        &app,
                        &document,
                        &selection,
                        &bitmap_shape,
                        &output_path,
                        bitmap.get_i32("SizeWidth")?,
                        bitmap.get_i32("SizeHeight")?,
                        bitmap.get_i32("ResolutionX")?,
                        bitmap.get_i32("ResolutionY")?,
                    )?;
                    let roundtrip = image::ImageReader::open(output_path)?
                        .decode()?
                        .into_rgba8();
                    let alphas = roundtrip.pixels().map(|pixel| pixel.0[3]);
                    let mut has_transparent = false;
                    let mut has_opaque = false;
                    let mut has_soft_edge = false;
                    for alpha in alphas {
                        has_transparent |= alpha == 0;
                        has_opaque |= alpha == 255;
                        has_soft_edge |= alpha > 0 && alpha < 255;
                    }
                    if !(has_transparent && has_opaque && has_soft_edge) {
                        return Ok(false);
                    }

                    let holes = super::process_selection_with_progress(
                        super::ProcessingSettings::default(),
                        super::SelectionOperation::AddHoles,
                        None,
                        |_, _| {},
                    )?;
                    if holes.processed_count != 1 {
                        return Err(std::io::Error::other(format!(
                            "the composite trim result must remain usable by AddHoles; processed {} bitmaps",
                            holes.processed_count
                        ))
                        .into());
                    }
                    let mut hole = None;
                    let layers = page.get_dispatch("Layers")?;
                    for layer_index in 1..=layers.get_i32("Count")? {
                        let layer = layers.get_dispatch_item(layer_index)?;
                        let shapes = layer.get_dispatch("Shapes")?;
                        for shape_index in 1..=shapes.get_i32("Count")? {
                            let shape = shapes.get_dispatch_item(shape_index)?;
                            if shape.get_i32("Type")? == 3
                                && shape.get_string("Name")?.starts_with("CDR_RUST_HOLE_")
                            {
                                hole = Some(shape);
                            }
                        }
                    }
                    let hole = hole.ok_or_else(|| {
                        std::io::Error::other("AddHoles did not create a selectable hole curve")
                    })?;
                    composite.call("CreateSelection", Vec::new())?;
                    hole.call("AddToSelection", Vec::new())?;
                    let merged = super::process_selection_with_progress(
                        super::ProcessingSettings::default(),
                        super::SelectionOperation::MergeHoles,
                        None,
                        |_, _| {},
                    )?;
                    if merged.processed_count != 1
                        || super::find_bitmap_by_static_id(&page, bitmap_static_id)?.is_none()
                    {
                        return Err(std::io::Error::other(
                            "merging a hole from the composite trim result must retain its bitmap",
                        )
                        .into());
                    }
                    let expected_group_name = super::generated_shape_name(
                        super::SelectionOperation::TrimTransparent,
                        bitmap_static_id,
                    );
                    let mut final_composite = None;
                    let layers = page.get_dispatch("Layers")?;
                    for layer_index in 1..=layers.get_i32("Count")? {
                        let layer = layers.get_dispatch_item(layer_index)?;
                        let shapes = layer.get_dispatch("Shapes")?;
                        for shape_index in 1..=shapes.get_i32("Count")? {
                            let shape = shapes.get_dispatch_item(shape_index)?;
                            if shape.get_i32("Type")? == 7
                                && shape.get_string("Name")? == expected_group_name
                            {
                                final_composite = Some(shape);
                            }
                        }
                    }
                    let final_composite = final_composite.ok_or_else(|| {
                        std::io::Error::other("hole merge did not preserve the composite group")
                    })?;
                    let children = final_composite.get_dispatch("Shapes")?;
                    let group_has_bitmap =
                        super::find_bitmap_in_shapes(&children, bitmap_static_id)?.is_some();
                    let mut group_has_contour = false;
                    for child_index in 1..=children.get_i32("Count")? {
                        let child = children.get_dispatch_item(child_index)?;
                        group_has_contour |= child.get_i32("Type")? == 3
                            && child.get_string("Name")?.starts_with("CDR_RUST_MERGED_");
                    }
                    if !group_has_bitmap || !group_has_contour {
                        return Err(std::io::Error::other(format!(
                            "hole merge must keep the image and merged contour together; group children={}",
                            children.get_i32("Count")?
                        ))
                        .into());
                    }
                    Ok(true)
                })();
                let _ = document.put("Dirty", false.into());
                let close_result = document.call("Close", Vec::new());
                let has_soft_edge = trim_result?;
                close_result?;
                Ok(has_soft_edge)
            })();

            assert!(
                matches!(result, Ok(true)),
                "transparent cleanup must composite a smooth alpha boundary into one bitmap: {result:?}"
            );
        }

        fn create_test_curve(
            app: &super::Dispatch,
            document: &super::Dispatch,
            layer: &super::Dispatch,
            name: &str,
            points: &[(f64, f64)],
            units: super::DocumentUnits,
        ) -> super::Result<super::Dispatch> {
            let curve =
                app.call_dispatch("CreateCurve", vec![super::dispatch_variant(&document.0)])?;
            let ring = points
                .iter()
                .map(|(x, y)| super::PointMm { x: *x, y: *y })
                .collect::<Vec<_>>();
            super::append_ring(&curve, &ring, units)?;
            let shape =
                layer.call_dispatch("CreateCurve", vec![super::dispatch_variant(&curve.0)])?;
            shape.put("Name", name.into())?;
            shape
                .get_dispatch("Fill")?
                .call("ApplyNoFill", Vec::new())?;
            Ok(shape)
        }

        #[test]
        fn zero_hole_diameter_disables_holes() {
            let settings = ProcessingSettings {
                outline_offset_mm: 2.0,
                hole_diameter_mm: 0.0,
                hole_edge_clearance_mm: 2.0,
                tool_diameter_mm: 2.0,
                smoothing_mm: 0.02,
            };

            assert!(settings.validate().is_ok());
        }

        #[test]
        fn negative_hole_diameter_is_rejected() {
            let settings = ProcessingSettings {
                outline_offset_mm: 2.0,
                hole_diameter_mm: -0.1,
                hole_edge_clearance_mm: 2.0,
                tool_diameter_mm: 2.0,
                smoothing_mm: 0.02,
            };

            assert!(settings.validate().is_err());
        }

        #[test]
        fn negative_smoothing_is_rejected() {
            let settings = ProcessingSettings {
                outline_offset_mm: 2.0,
                hole_diameter_mm: 3.0,
                hole_edge_clearance_mm: 2.0,
                tool_diameter_mm: 2.0,
                smoothing_mm: -0.01,
            };

            assert!(settings.validate().is_err());
        }

        #[test]
        fn negative_tool_diameter_is_rejected() {
            let settings = ProcessingSettings {
                outline_offset_mm: 2.0,
                hole_diameter_mm: 3.0,
                hole_edge_clearance_mm: 2.0,
                tool_diameter_mm: -0.01,
                smoothing_mm: 0.02,
            };

            assert!(settings.validate().is_err());
        }
    }
}

#[cfg(windows)]
pub use windows_adapter::{
    ProcessOutcome, ProcessingSettings, SelectionOperation, process_active_selection,
    process_selection_with_progress, run_cli,
};

#[cfg(not(windows))]
pub fn run_cli() -> std::result::Result<(), Box<dyn std::error::Error>> {
    Err("cdr-corel requires Windows and CorelDRAW 2020".into())
}
