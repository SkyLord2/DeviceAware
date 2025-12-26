use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use windows::core::{GUID};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{
    PowerRegisterForEffectivePowerModeNotifications, PowerUnregisterFromEffectivePowerModeNotifications,
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification,
    EFFECTIVE_POWER_MODE, EFFECTIVE_POWER_MODE_V2,
    HPOWERNOTIFY, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
    POWERBROADCAST_SETTING,
};

use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE, };

use windows::Win32::System::SystemServices::{
    GUID_POWER_SAVING_STATUS, GUID_ACDC_POWER_SOURCE,
};

// ============================================================================
// 辅助类型与描述
// ============================================================================

#[derive(Debug, PartialEq, Clone, Copy)]
enum PowerSourceType {
    AC = 0,
    Battery = 1,
    ShortTerm = 2,
    Unknown = -1,
}

impl From<u32> for PowerSourceType {
    fn from(val: u32) -> Self {
        match val {
            0 => PowerSourceType::AC,
            1 => PowerSourceType::Battery,
            2 => PowerSourceType::ShortTerm,
            _ => PowerSourceType::Unknown,
        }
    }
}

fn describe_effective_mode(mode: EFFECTIVE_POWER_MODE) -> String {
    match mode.0 {
        0 => "滑块: 最左 (节电)".to_string(),
        1 => "滑块: 较左 (更好电池)".to_string(),
        2 => "滑块: 中间 (平衡)".to_string(),
        3 => "滑块: 较右 (最佳性能)".to_string(),
        4 => "滑块: 最右 (最大性能)".to_string(),
        5 => "滑块: 游戏模式".to_string(),
        _ => "滑块: 未知".to_string(),
    }
}

fn describe_power_source(source: PowerSourceType) -> String {
    match source {
        PowerSourceType::AC => "电源: 电源供电".to_string(),
        PowerSourceType::Battery => "电源: 电池供电".to_string(),
        PowerSourceType::ShortTerm => "电源: 短期/UPS".to_string(),
        PowerSourceType::Unknown => "电源: 未知".to_string(),
    }
}

fn describe_saver_status(is_on: bool) -> String {
    if is_on {
        "节电模式: [已开启] (建议减少后台活动)".to_string()
    } else {
        "节电模式: [未开启]".to_string()
    }
}

// ============================================================================
// 1. EffectiveModeObserver (修复版)
// ============================================================================

// 定义回调类型别名，方便处理
type EffectiveModeCallback = Box<dyn Fn(EFFECTIVE_POWER_MODE) + Send + Sync>;

struct EffectiveModeObserver {
    handle: *mut c_void,
    // 我们保存原始指针，以便在 Drop 时将其转回 Box 进行释放
    raw_context: *mut EffectiveModeCallback, 
}

impl EffectiveModeObserver {
    pub fn new<F>(handler: F) -> Self 
    where F: Fn(EFFECTIVE_POWER_MODE) + Send + Sync + 'static 
    {
        // 1. 创建闭包的胖指针 Box<dyn Fn>
        let callback: EffectiveModeCallback = Box::new(handler);
        
        // 2. 将胖指针装入外层 Box，并转换为原始指针 (Double Boxing)
        // 这样 raw_context 就是一个指向 "Box<dyn Fn>" 的瘦指针 (8 bytes)，适合传给 void*
        let raw_context = Box::into_raw(Box::new(callback));
        
        let mut handle = std::ptr::null_mut();

        unsafe {
            let hr = PowerRegisterForEffectivePowerModeNotifications(
                EFFECTIVE_POWER_MODE_V2,
                Some(Self::static_cb),
                Some(raw_context as *const c_void), // 传入瘦指针
                &mut handle,
            );

            if hr.is_err() {
                eprintln!("PowerRegisterForEffectivePowerModeNotifications failed");
                // 如果注册失败，我们需要手动回收内存，否则泄漏
                let _ = Box::from_raw(raw_context); 
            }
        }

        EffectiveModeObserver {
            handle,
            raw_context,
        }
    }

    unsafe extern "system" fn static_cb(mode: EFFECTIVE_POWER_MODE, context: *const c_void) {
        if !context.is_null() {
            // 3. 将 void* 转回为指向 EffectiveModeCallback 的指针
            let cb_ptr: *const Box<dyn Fn(EFFECTIVE_POWER_MODE) + Send + Sync> = context as *const EffectiveModeCallback;
            // 4. 解引用得到 &Box<dyn Fn>，再调用
            unsafe {
                (*cb_ptr)(mode);
            }
        }
    }
}

impl Drop for EffectiveModeObserver {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = PowerUnregisterFromEffectivePowerModeNotifications(self.handle);
                // 5. 关键：手动回收内存。将 raw pointer 转回 Box，离开作用域自动释放。
                let _ = Box::from_raw(self.raw_context);
            }
        }
    }
}

// ============================================================================
// 2. PowerSettingObserver (修复版)
// ============================================================================

type PowerSettingCallback = Box<dyn Fn(u32) + Send + Sync>;

struct PowerSettingObserver {
    handle: Option<HPOWERNOTIFY>, 
    raw_context: *mut PowerSettingCallback,
}

impl PowerSettingObserver {
    pub fn new<F>(guid: GUID, handler: F) -> Self
    where F: Fn(u32) + Send + Sync + 'static
    {
        // 1. Double Boxing 策略
        let callback: PowerSettingCallback = Box::new(handler);
        let raw_context = Box::into_raw(Box::new(callback));

        // 2. 这里的 Context 必须是指向我们堆内存的指针
        let mut params = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(Self::static_callback),
            Context: raw_context as *mut c_void, 
        };

        let result = unsafe {
            RegisterPowerSettingNotification(
                HANDLE(&mut params as *mut _ as *mut c_void),
                &guid,
                DEVICE_NOTIFY_CALLBACK, 
            )
        };
        
        let handle = match result {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("RegisterPowerSettingNotification failed for GUID {:?}: {:?}", guid, e);
                unsafe { let _ = Box::from_raw(raw_context); } // 失败回滚
                None
            }
        };

        PowerSettingObserver {
            handle,
            raw_context,
        }
    }

    unsafe extern "system" fn static_callback(
        context: *const c_void,
        type_: u32,
        setting: *const c_void,
    ) -> u32 {
        if type_ == PBT_POWERSETTINGCHANGE && !context.is_null() && !setting.is_null() {
            let p_setting = unsafe { &*(setting as *const POWERBROADCAST_SETTING) };
            
            if p_setting.DataLength == std::mem::size_of::<u32>() as u32 {
                // ---------------- 修复开始 ----------------
                
                // 1. 获取 Data 字段的首地址指针
                let data_ptr = p_setting.Data.as_ptr();

                // 2. 根据 DataLength (4) 手动构建切片，绕过 [u8; 1] 的静态限制
                let data_slice = unsafe { std::slice::from_raw_parts(data_ptr, p_setting.DataLength as usize) };
                
                // 3. 安全转换 (这里就不需要 try_into 导致的 panic 风险了)
                let val = u32::from_ne_bytes(data_slice.try_into().unwrap_or([0, 0, 0, 0]));
                
                // ---------------- 修复结束 ----------------
                
                // 3. 恢复指针并调用
                let cb_ptr: *const Box<dyn Fn(u32) + Send + Sync> = context as *const PowerSettingCallback;
                unsafe {
                    (*cb_ptr)(val);
                }
            }
        }
        0 
    }
}

impl Drop for PowerSettingObserver {
    fn drop(&mut self) {
        if let Some(h) = self.handle {
            unsafe {
                let _ = UnregisterPowerSettingNotification(h);
                // 4. 回收内存
                let _ = Box::from_raw(self.raw_context);
            }
        }
    }
}

// ============================================================================
// 业务入口 (main)
// ============================================================================

fn main() {
    let io_mutex = Arc::new(Mutex::new(()));

    // 使用 Arc 克隆引用，因为闭包需要 'static 生命周期
    let safe_print = move |msg: String| {
        let _lock = io_mutex.lock().unwrap();
        println!("{}", msg);
    };

    println!("启动全维度电源监控 (AC/DC + 滑块 + 节电模式)...");
    println!("--------------------------------------------------");

    let sp1 = safe_print.clone();
    let _perf_obs = EffectiveModeObserver::new(move |mode| {
        sp1(describe_effective_mode(mode));
    });

    let sp2 = safe_print.clone();
    let _source_obs = PowerSettingObserver::new(GUID_ACDC_POWER_SOURCE, move |val| {
        let source = PowerSourceType::from(val);
        sp2(describe_power_source(source));
    });

    let sp3 = safe_print.clone();
    let _saver_obs = PowerSettingObserver::new(GUID_POWER_SAVING_STATUS, move |val| {
        let is_on = val != 0;
        sp3(describe_saver_status(is_on));
    });

    loop {
        thread::sleep(Duration::from_secs(1));
    }
}