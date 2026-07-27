#![windows_subsystem = "windows"]

use eframe::egui;
use image::{GenericImageView, Rgba};
use image::imageops::FilterType;
use imageproc::drawing::draw_text_mut;
use rusttype::{Font, Scale};

use serde::Deserialize;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics,
    SystemParametersInfoW,
    SM_CXSCREEN,
    SM_CYSCREEN,
    SPIF_SENDWININICHANGE,
    SPIF_UPDATEINIFILE,
    SPI_SETDESKWALLPAPER,
};

use wmi::{COMLibrary, WMIConnection};


/// 单张网卡
///
/// 一张网卡可以拥有多个 IP，因此 ips 使用 Vec<String>。
#[derive(Clone, Default)]
struct NetAdapter {
    /// 网卡显示名称，例如：
    /// Ethernet、Wi-Fi、VMware VMnet8、Company VPN
    name: String,

    /// 网卡 MAC 地址
    mac: String,

    /// 该网卡上的多个 IPv4 地址
    ips: Vec<String>,

    /// 是否在水印中显示
    selected: bool,
}

impl NetAdapter {
    fn ips_line(&self) -> String {
        self.ips.join("  ")
    }
}


/// 网络信息
#[derive(Clone, Default)]
struct NetworkInfo {
    hostname: String,
    adapters: Vec<NetAdapter>,
    user_path: String,
}


/// 水印显示选项
#[derive(Clone)]
struct WatermarkOptions {
    show_ip: bool,
    show_mac: bool,
    show_hostname: bool,
    remark: String,
}


/// 判断字符串是否为 IPv4 地址
fn is_ipv4(value: &str) -> bool {
    value.parse::<Ipv4Addr>().is_ok()
}


/// 获取当前网络信息
///
/// 这里不再只筛选 PCI/USB 物理网卡，
/// 因此 VMware、VPN、虚拟网卡等接口也可以显示。
fn get_network_info() -> Result<NetworkInfo, Box<dyn std::error::Error>> {
    use std::thread;
    use std::time::Duration;

    let user_path =
        env::var("USERPROFILE").unwrap_or_else(|_| r"C:\".to_string());

    let hostname =
        env::var("COMPUTERNAME").unwrap_or_else(|_| "Unknown".to_string());

    #[derive(Deserialize, Debug)]
    struct Win32NetworkAdapterConfiguration {
        #[serde(rename = "MACAddress")]
        mac_address: Option<String>,

        #[serde(rename = "IPEnabled")]
        ip_enabled: Option<bool>,

        #[serde(rename = "IPAddress")]
        ip_address: Option<Vec<String>>,

        #[serde(rename = "Description")]
        description: Option<String>,
    }

    let mut last_error = String::new();

    // WMI 偶发失败时重试几次
    for attempt in 0..3 {
        match query_network_adapters::<Win32NetworkAdapterConfiguration>() {
            Ok(configs) => {
                let mut result: Vec<NetAdapter> = Vec::new();

                for config in configs {
                    // 只处理启用 IP 的接口
                    if !config.ip_enabled.unwrap_or(false) {
                        continue;
                    }

                    let mac = match config.mac_address {
                        Some(value) if !value.trim().is_empty() => value,
                        _ => continue,
                    };

                    let mut ips: Vec<String> = Vec::new();

                    if let Some(ip_list) = config.ip_address {
                        for ip in ip_list {
                            let ip = ip.trim().to_string();

                            // 只保留 IPv4，自动排除 IPv6 和 fe80:: 地址
                            if is_ipv4(&ip) {
                                ips.push(ip);
                            }
                        }
                    }

                    // 默认过滤无 IPv4 地址的接口
                    if ips.is_empty() {
                        continue;
                    }

                    ips.sort();
                    ips.dedup();

                    let name = config
                        .description
                        .unwrap_or_else(|| "未知网络接口".to_string());

                    result.push(NetAdapter {
                        name,
                        mac,
                        ips,
                        selected: false,
                    });
                }

                // 排序，保证显示顺序稳定
                result.sort_by(|a, b| {
                    a.name
                        .to_lowercase()
                        .cmp(&b.name.to_lowercase())
                });

                // 默认勾选第一张有 IP 的网卡
                if let Some(first) = result.first_mut() {
                    first.selected = true;
                }

                return Ok(NetworkInfo {
                    hostname,
                    adapters: result,
                    user_path,
                });
            }

            Err(error) => {
                last_error = error.to_string();

                if attempt < 2 {
                    thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    Err(format!(
        "WMI 查询网络接口失败，已重试 3 次：{}",
        last_error
    )
    .into())
}


/// 执行一次 WMI 网络接口查询
fn query_network_adapters<T>() -> Result<Vec<T>, Box<dyn std::error::Error>>
where
    T: for<'de> Deserialize<'de>,
{
    let com_con = COMLibrary::new()?;
    let wmi_con = WMIConnection::new(com_con)?;

    let configs: Vec<T> = wmi_con.raw_query(
        "SELECT MACAddress, IPEnabled, IPAddress, Description \
         FROM Win32_NetworkAdapterConfiguration \
         WHERE IPEnabled = TRUE"
    )?;

    Ok(configs)
}


/// 设置系统壁纸
fn set_wallpaper(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let full_path = fs::canonicalize(path)?;

    let path_str = full_path
        .to_str()
        .ok_or("路径包含非法字符")?
        .trim_start_matches(r"\\?\")
        .to_string();

    let path_hstring = HSTRING::from(&path_str);

    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(path_hstring.as_ptr() as *const _ as *mut _),
            SPIF_UPDATEINIFILE | SPIF_SENDWININICHANGE,
        )?;
    }

    Ok(())
}


/// 根据选择的网卡生成水印文本
fn build_watermark_lines(
    info: &NetworkInfo,
    options: &WatermarkOptions,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    if options.show_hostname {
        lines.push(format!("计算机名：{}", info.hostname));
    }

    let selected_adapters: Vec<&NetAdapter> = info
        .adapters
        .iter()
        .filter(|adapter| adapter.selected)
        .collect();

    // 显示 IP
    if options.show_ip {
        let mut first_ip = true;

        for adapter in &selected_adapters {
            if adapter.ips.is_empty() {
                continue;
            }

            let ip_text = adapter.ips.join("  ");

            if first_ip {
                lines.push(format!(
                    "IP 地址（{}）：{}",
                    adapter.name,
                    ip_text
                ));
                first_ip = false;
            } else {
                lines.push(format!(
                    "          （{}）：{}",
                    adapter.name,
                    ip_text
                ));
            }
        }
    }

    // 显示 MAC
    if options.show_mac {
        let mut first_mac = true;

        for adapter in &selected_adapters {
            if adapter.mac.trim().is_empty() {
                continue;
            }

            if first_mac {
                lines.push(format!(
                    "MAC 地址（{}）：{}",
                    adapter.name,
                    adapter.mac
                ));
                first_mac = false;
            } else {
                lines.push(format!(
                    "           （{}）：{}",
                    adapter.name,
                    adapter.mac
                ));
            }
        }
    }

    // 显示备注
    if !options.remark.trim().is_empty() {
        for (index, line) in options.remark.lines().enumerate() {
            if index == 0 {
                lines.push(format!("备注：    {}", line));
            } else {
                lines.push(format!("          {}", line));
            }
        }
    }

    lines
}


/// 创建带水印的壁纸
fn create_watermark(
    info: &NetworkInfo,
    options: &WatermarkOptions,
    font: &Font,
) -> Result<(), Box<dyn std::error::Error>> {
    // ---------------------------------------------------------------------
    // 1. 获取屏幕分辨率
    // ---------------------------------------------------------------------
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) } as u32;
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) } as u32;

    if screen_w == 0 || screen_h == 0 {
        return Err("无法获取屏幕分辨率".into());
    }

    // ---------------------------------------------------------------------
    // 2. 路径准备
    // ---------------------------------------------------------------------
    let user_path = if info.user_path.is_empty() {
        env::var("USERPROFILE")
            .unwrap_or_else(|_| r"C:\".to_string())
    } else {
        info.user_path.clone()
    };

    let theme_dir = PathBuf::from(&user_path)
        .join(r"AppData\Roaming\Microsoft\Windows\Themes");

    let wallpaper_path = theme_dir.join("TranscodedWallpaper");

    let output_dir = PathBuf::from(&user_path)
        .join("WallpaperTool");

    let output_path = output_dir.join("Wallpaper_Watermark.jpg");
    let backup_path = output_dir.join("Wallpaper_Backup.jpg");

    if !output_dir.exists() {
        fs::create_dir_all(&output_dir)?;
    }

    // ---------------------------------------------------------------------
    // 3. 备份原壁纸
    // ---------------------------------------------------------------------
    if !wallpaper_path.exists() {
        return Err(
            "未找到系统壁纸 TranscodedWallpaper，请确保当前使用的是图片壁纸。"
                .into(),
        );
    }

    fs::copy(&wallpaper_path, &backup_path)?;

    let img_data = fs::read(&wallpaper_path).map_err(|e| {
        format!("无法读取壁纸：{}，请稍后重试。", e)
    })?;

    // ---------------------------------------------------------------------
    // 4. 准备水印文本
    // ---------------------------------------------------------------------
    let lines = build_watermark_lines(info, options);

    if lines.is_empty() {
        return Err("没有可显示的水印内容".into());
    }

    // ---------------------------------------------------------------------
    // 5. 读取原壁纸
    // ---------------------------------------------------------------------
    let img = image::load_from_memory(&img_data)?;
    let (img_w, img_h) = img.dimensions();

    if img_w == 0 || img_h == 0 {
        return Err("原壁纸尺寸无效".into());
    }

    // ---------------------------------------------------------------------
    // 6. 按 Windows 填充模式缩放
    // ---------------------------------------------------------------------
    let scale = f32::max(
        screen_w as f32 / img_w as f32,
        screen_h as f32 / img_h as f32,
    );

    let new_w = (img_w as f32 * scale) as u32;
    let new_h = (img_h as f32 * scale) as u32;

    let img_resized = img.resize_exact(
        new_w,
        new_h,
        FilterType::Triangle,
    );

    let mut canvas = image::RgbaImage::new(screen_w, screen_h);

    let offset_x = (screen_w as i64 - new_w as i64) / 2;
    let offset_y = (screen_h as i64 - new_h as i64) / 2;

    image::imageops::overlay(
        &mut canvas,
        &img_resized.to_rgba8(),
        offset_x,
        offset_y,
    );

    // ---------------------------------------------------------------------
    // 7. 计算字体大小和右上角位置
    // ---------------------------------------------------------------------
    let font_size = (screen_h as f32 / 40.0)
        .max(14.0)
        .round();

    let scale_font = Scale::uniform(font_size);

    let margin_right = (screen_w as f32 * 0.05) as i32;
    let margin_top = (screen_h as f32 * 0.05) as i32;

    let mut max_width = 0.0f32;

    for line in &lines {
        let glyphs: Vec<_> = font
            .layout(
                line,
                scale_font,
                rusttype::point(0.0, 0.0),
            )
            .collect();

        let width = glyphs
            .iter()
            .rev()
            .next()
            .map(|glyph| {
                glyph.position().x
                    + glyph.unpositioned().h_metrics().advance_width
            })
            .unwrap_or(0.0);

        if width > max_width {
            max_width = width;
        }
    }

    let start_x = (
        screen_w as f32
            - max_width
            - margin_right as f32
    ) as i32;

    let start_x = start_x.max(10);

    // ---------------------------------------------------------------------
    // 8. 绘制文字
    // ---------------------------------------------------------------------
    for (index, line) in lines.iter().enumerate() {
        let y = margin_top
            + (index as f32 * font_size * 1.3) as i32;

        // 黑色阴影
        draw_text_mut(
            &mut canvas,
            Rgba([0, 0, 0, 180]),
            start_x + 2,
            y + 2,
            scale_font,
            font,
            line,
        );

        // 白色正文
        draw_text_mut(
            &mut canvas,
            Rgba([255, 255, 255, 255]),
            start_x,
            y,
            scale_font,
            font,
            line,
        );
    }

    // ---------------------------------------------------------------------
    // 9. 保存 JPG
    // ---------------------------------------------------------------------
    {
        let file = fs::File::create(&output_path)?;

        let writer = std::io::BufWriter::with_capacity(
            512 * 1024,
            file,
        );

        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(
                writer,
                95,
            );

        encoder.encode_image(&canvas)?;
    }

    // ---------------------------------------------------------------------
    // 10. 应用壁纸
    // ---------------------------------------------------------------------
    set_wallpaper(&output_path)?;

    Ok(())
}


/// GUI 应用
struct WatermarkApp {
    options: WatermarkOptions,
    status: Arc<Mutex<String>>,
    drawing_font: Arc<Font<'static>>,
    exit_timer: Option<Instant>,
    network_info: NetworkInfo,
}

impl WatermarkApp {
    fn new(font: Arc<Font<'static>>) -> Self {
        let mut info = get_network_info().unwrap_or_else(|_| {
            NetworkInfo {
                user_path: env::var("USERPROFILE")
                    .unwrap_or_default(),

                hostname: env::var("COMPUTERNAME")
                    .unwrap_or_default(),

                ..Default::default()
            }
        });

        if info.user_path.is_empty() {
            info.user_path = env::var("USERPROFILE")
                .unwrap_or_else(|_| r"C:\".to_string());
        }

        Self {
            options: WatermarkOptions {
                show_ip: true,
                show_mac: true,
                show_hostname: true,
                remark: String::new(),
            },

            status: Arc::new(Mutex::new(
                "就绪".to_string()
            )),

            drawing_font: font,
            exit_timer: None,
            network_info: info,
        }
    }
}

impl eframe::App for WatermarkApp {
    fn update(
        &mut self,
        ctx: &egui::Context,
        _frame: &mut eframe::Frame,
    ) {
        {
            let current_status = self.status.lock().unwrap();

            if (*current_status == "SUCCESS"
                || *current_status == "RESTORE_SUCCESS")
                && self.exit_timer.is_none()
            {
                self.exit_timer = Some(Instant::now());
            }
        }

        let mut display_msg =
            self.status.lock().unwrap().clone();

        if let Some(start_time) = self.exit_timer {
            let elapsed = start_time.elapsed().as_secs();

            if elapsed >= 5 {
                ctx.send_viewport_cmd(
                    egui::ViewportCommand::Close
                );
            } else {
                ctx.request_repaint_after(
                    Duration::from_millis(100)
                );

                if display_msg == "SUCCESS"
                    || display_msg == "RESTORE_SUCCESS"
                {
                    let prefix =
                        if display_msg == "SUCCESS" {
                            "应用成功！"
                        } else {
                            "还原成功！"
                        };

                    display_msg = format!(
                        "{} {} 秒后自动退出...",
                        prefix,
                        5 - elapsed
                    );
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(5.0);

            // -------------------------------------------------------------
            // 水印内容选项
            // -------------------------------------------------------------
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut self.options.show_ip,
                    "IP 地址",
                );

                ui.checkbox(
                    &mut self.options.show_mac,
                    "MAC 地址",
                );

                ui.checkbox(
                    &mut self.options.show_hostname,
                    "主机名",
                );
            });

            ui.add_space(8.0);

            // -------------------------------------------------------------
            // 网卡选择
            // -------------------------------------------------------------
            ui.label(
                egui::RichText::new("选择要显示的网络接口")
                    .strong(),
            );

            ui.separator();

            if self.network_info.adapters.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "未发现有 IPv4 地址的网络接口",
                    )
                    .color(egui::Color32::RED),
                );
            } else {
                egui::ScrollArea::vertical()
                    .max_height(150.0)
                    .show(ui, |ui| {
                        for adapter
                            in &mut self.network_info.adapters
                        {
                            ui.horizontal(|ui| {
                                // 一张网卡只有一个勾选框
                                ui.checkbox(
                                    &mut adapter.selected,
                                    "",
                                );

                                // 网卡名称
                                ui.add_sized(
                                    [145.0, 20.0],
                                    egui::Label::new(
                                        egui::RichText::new(
                                            &adapter.name,
                                        )
                                        .strong(),
                                    ),
                                );

                                // 同一张网卡的多个 IP
                                ui.monospace(
                                    adapter.ips_line(),
                                );
                            });
                        }
                    });
            }

            ui.separator();

            // -------------------------------------------------------------
            // 备注
            // -------------------------------------------------------------
            ui.horizontal(|ui| {
                ui.label("备注：");

                ui.add(
                    egui::TextEdit::singleline(
                        &mut self.options.remark,
                    )
                    .desired_width(270.0),
                );
            });

            ui.add_space(10.0);

            // -------------------------------------------------------------
            // 操作按钮
            // -------------------------------------------------------------
            ui.horizontal(|ui| {
                if ui.button("应用").clicked() {
                    self.exit_timer = None;

                    // 至少选择一项水印内容
                    if !self.options.show_ip
                        && !self.options.show_mac
                        && !self.options.show_hostname
                        && self.options.remark.trim().is_empty()
                    {
                        *self.status.lock().unwrap() =
                            "错误：请至少勾选一项信息或填写备注"
                                .to_string();

                        return;
                    }

                    // 如果需要显示 IP 或 MAC，至少选择一张网卡
                    if (self.options.show_ip
                        || self.options.show_mac)
                        && !self
                            .network_info
                            .adapters
                            .iter()
                            .any(|adapter| adapter.selected)
                    {
                        *self.status.lock().unwrap() =
                            "错误：请至少选择一张网络接口"
                                .to_string();

                        return;
                    }

                    let status =
                        Arc::clone(&self.status);

                    let opts =
                        self.options.clone();

                    let font_clone =
                        Arc::clone(&self.drawing_font);

                    let ctx_clone = ctx.clone();

                    // 判断缓存中的网卡信息是否有效
                    let has_valid_data =
                        self.network_info
                            .adapters
                            .iter()
                            .any(|adapter| !adapter.ips.is_empty());

                    let cached_info =
                        self.network_info.clone();

                    thread::spawn(move || {
                        let final_info;

                        if has_valid_data {
                            *status.lock().unwrap() =
                                "正在处理...".to_string();

                            final_info = cached_info;
                        } else {
                            *status.lock().unwrap() =
                                "正在刷新网络信息..."
                                    .to_string();

                            ctx_clone.request_repaint();

                            final_info =
                                get_network_info()
                                    .unwrap_or(cached_info);

                            *status.lock().unwrap() =
                                "正在处理...".to_string();

                            ctx_clone.request_repaint();
                        }

                        match create_watermark(
                            &final_info,
                            &opts,
                            &font_clone,
                        ) {
                            Ok(_) => {
                                *status.lock().unwrap() =
                                    "SUCCESS".to_string();
                            }

                            Err(error) => {
                                *status.lock().unwrap() =
                                    format!("失败：{}", error);
                            }
                        }

                        ctx_clone.request_repaint();
                    });
                }

                if ui.button("刷新网卡").clicked() {
                    match get_network_info() {
                        Ok(mut info) => {
                            // 刷新后默认选中第一张网卡
                            for adapter
                                in &mut info.adapters
                            {
                                adapter.selected = false;
                            }

                            if let Some(first) =
                                info.adapters.first_mut()
                            {
                                first.selected = true;
                            }

                            self.network_info = info;

                            *self.status.lock().unwrap() =
                                "网络接口已刷新".to_string();
                        }

                        Err(error) => {
                            *self.status.lock().unwrap() =
                                format!(
                                    "刷新失败：{}",
                                    error
                                );
                        }
                    }
                }

                if ui.button("清除").clicked() {
                    self.options.show_ip = false;
                    self.options.show_mac = false;
                    self.options.show_hostname = false;
                    self.options.remark.clear();

                    self.exit_timer = None;

                    *self.status.lock().unwrap() =
                        "配置已重置".to_string();
                }

                if ui.button("还原").clicked() {
                    let user_path =
                        env::var("USERPROFILE")
                            .unwrap_or_default();

                    let backup = PathBuf::from(user_path)
                        .join("WallpaperTool")
                        .join("Wallpaper_Backup.jpg");

                    if backup.exists() {
                        match set_wallpaper(&backup) {
                            Ok(_) => {
                                *self.status.lock().unwrap() =
                                    "RESTORE_SUCCESS".to_string();

                                self.exit_timer =
                                    Some(Instant::now());
                            }

                            Err(error) => {
                                *self.status.lock().unwrap() =
                                    format!(
                                        "还原失败：{}",
                                        error
                                    );
                            }
                        }
                    } else {
                        *self.status.lock().unwrap() =
                            "未找到壁纸备份文件".to_string();
                    }
                }
            });

            ui.add_space(8.0);
            ui.separator();

            // -------------------------------------------------------------
            // 状态栏
            // -------------------------------------------------------------
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(display_msg)
                        .size(12.0)
                        .color(
                            egui::Color32::from_rgb(
                                160,
                                160,
                                160,
                            ),
                        ),
                );

                ui.with_layout(
                    egui::Layout::right_to_left(
                        egui::Align::Center,
                    ),
                    |ui| {
                        ui.spacing_mut()
                            .item_spacing
                            .x = 0.0;

                        ui.hyperlink_to(
                            egui::RichText::new("关于")
                                .size(12.0),
                            "https://github.com/ljx8090/WallpaperTool",
                        );
                    },
                );
            });
        });
    }
}


/// 程序入口
fn main() -> Result<(), eframe::Error> {
    // 设置 DPI 感知
    unsafe {
        let _ =
            windows::Win32::UI::HiDpi::
                SetProcessDpiAwarenessContext(
                    windows::Win32::UI::HiDpi::
                        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                );
    }

    // 加载中文字体
    let font_data = fs::read(
        r"C:\Windows\Fonts\msyh.ttc",
    )
    .or_else(|_| {
        fs::read(r"C:\Windows\Fonts\simhei.ttf")
    })
    .expect("Font not found");

    let drawing_font =
        Arc::new(Font::try_from_vec(font_data.clone())
            .expect("无法加载绘图字体"));

    // -------------------------------------------------------------
    // 静默模式
    // -------------------------------------------------------------
    let args: Vec<String> = env::args().collect();

    if args.iter().any(|arg| {
        arg == "-q"
            || arg == "/q"
            || arg == "q"
    }) {
        let mut info =
            get_network_info().unwrap_or_else(|_| {
                NetworkInfo {
                    user_path: env::var(
                        "USERPROFILE",
                    )
                    .unwrap_or_default(),

                    hostname: env::var(
                        "COMPUTERNAME",
                    )
                    .unwrap_or_default(),

                    ..Default::default()
                }
            });

        if info.user_path.is_empty() {
            info.user_path =
                env::var("USERPROFILE")
                    .unwrap_or_else(|_| r"C:\".to_string());
        }

        // 静默模式默认选择全部有 IP 的网卡
        for adapter in &mut info.adapters {
            adapter.selected = true;
        }

        let default_options = WatermarkOptions {
            show_ip: true,
            show_mac: true,
            show_hostname: true,
            remark: String::new(),
        };

        let _ = create_watermark(
            &info,
            &default_options,
            &drawing_font,
        );

        return Ok(());
    }

    // -------------------------------------------------------------
    // GUI 图标
    // -------------------------------------------------------------
    let icon_data = include_bytes!("./ip.png");

    let icon = image::load_from_memory(icon_data)
        .expect("Failed to load icon")
        .to_rgba8();

    let (icon_width, icon_height) =
        icon.dimensions();

    let window_icon = egui::IconData {
        rgba: icon.into_raw(),
        width: icon_width,
        height: icon_height,
    };

    // -------------------------------------------------------------
    // GUI 配置
    // -------------------------------------------------------------
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 390.0])
            .with_resizable(false)
            .with_icon(window_icon),

        ..Default::default()
    };

    eframe::run_native(
        "壁纸水印工具",
        native_options,
        Box::new(move |cc| {
            let mut fonts =
                egui::FontDefinitions::default();

            fonts.font_data.insert(
                "chinese_font".to_owned(),
                egui::FontData::from_owned(font_data),
            );

            fonts
                .families
                .get_mut(&egui::FontFamily::Proportional)
                .unwrap()
                .insert(0, "chinese_font".to_owned());

            fonts
                .families
                .get_mut(&egui::FontFamily::Monospace)
                .unwrap()
                .insert(0, "chinese_font".to_owned());

            cc.egui_ctx.set_fonts(fonts);

            Box::new(WatermarkApp::new(
                drawing_font,
            ))
        }),
    )
}
