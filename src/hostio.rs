//! Host terminal helpers: IME composition and cursor-preserving cell writes.

use std::io;

/// True while an IME composition string is open (not yet committed into the app).
pub fn ime_composing() -> bool {
    #[cfg(windows)]
    {
        ime_composing_windows()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Write characters without moving the console cursor.
/// Returns `true` when the host applied the patch in-place.
pub fn patch_chars_keep_cursor(cells: &[(u16, u16, char)]) -> io::Result<bool> {
    if cells.is_empty() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        patch_chars_windows(cells)
    }
    #[cfg(not(windows))]
    {
        let _ = cells;
        Ok(false)
    }
}

#[cfg(windows)]
fn ime_composing_windows() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::Input::Ime::{
        ImmGetCompositionStringW, ImmGetContext, ImmGetDefaultIMEWnd, ImmReleaseContext,
        GCS_COMPSTR,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    unsafe {
        let console = GetConsoleWindow();
        let ime_wnd = if console.is_null() {
            std::ptr::null_mut()
        } else {
            ImmGetDefaultIMEWnd(console)
        };
        for hwnd in [console, ime_wnd, GetForegroundWindow()] {
            if hwnd.is_null() {
                continue;
            }
            let himc = ImmGetContext(hwnd);
            if himc.is_null() {
                continue;
            }
            let len = ImmGetCompositionStringW(himc, GCS_COMPSTR, std::ptr::null_mut(), 0);
            ImmReleaseContext(hwnd, himc);
            if len > 0 {
                return true;
            }
        }
    }
    false
}

#[cfg(windows)]
fn patch_chars_windows(cells: &[(u16, u16, char)]) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleOutputW, SetConsoleCursorPosition,
        WriteConsoleOutputW, CHAR_INFO, CHAR_INFO_0, CONSOLE_SCREEN_BUFFER_INFO, COORD, SMALL_RECT,
        STD_OUTPUT_HANDLE,
    };

    unsafe {
        let h: HANDLE = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Ok(false);
        }
        let mut info = CONSOLE_SCREEN_BUFFER_INFO {
            dwSize: COORD { X: 0, Y: 0 },
            dwCursorPosition: COORD { X: 0, Y: 0 },
            wAttributes: 0,
            srWindow: SMALL_RECT {
                Left: 0,
                Top: 0,
                Right: 0,
                Bottom: 0,
            },
            dwMaximumWindowSize: COORD { X: 0, Y: 0 },
        };
        if GetConsoleScreenBufferInfo(h, &mut info) == 0 {
            return Ok(false);
        }
        let saved = info.dwCursorPosition;
        let ox = info.srWindow.Left;
        let oy = info.srWindow.Top;

        let mut sorted = cells.to_vec();
        sorted.sort_by_key(|(x, y, _)| (*y, *x));
        let mut i = 0;
        while i < sorted.len() {
            let y = sorted[i].1;
            let x0 = sorted[i].0;
            let mut end = i + 1;
            while end < sorted.len()
                && sorted[end].1 == y
                && sorted[end].0 == sorted[end - 1].0.saturating_add(1)
            {
                end += 1;
            }
            let n = (end - i) as i16;
            if n <= 0 {
                i = end;
                continue;
            }
            let mut buf = vec![
                CHAR_INFO {
                    Char: CHAR_INFO_0 { UnicodeChar: 32 },
                    Attributes: 0,
                };
                n as usize
            ];
            let mut region = SMALL_RECT {
                Left: ox.saturating_add(x0 as i16),
                Top: oy.saturating_add(y as i16),
                Right: ox.saturating_add(x0 as i16).saturating_add(n - 1),
                Bottom: oy.saturating_add(y as i16),
            };
            let size = COORD { X: n, Y: 1 };
            let origin = COORD { X: 0, Y: 0 };
            let _ = ReadConsoleOutputW(h, buf.as_mut_ptr(), size, origin, &mut region);
            for (j, (_, _, ch)) in sorted[i..end].iter().enumerate() {
                buf[j].Char.UnicodeChar = *ch as u16;
            }
            let mut write_region = SMALL_RECT {
                Left: ox.saturating_add(x0 as i16),
                Top: oy.saturating_add(y as i16),
                Right: ox.saturating_add(x0 as i16).saturating_add(n - 1),
                Bottom: oy.saturating_add(y as i16),
            };
            if WriteConsoleOutputW(h, buf.as_ptr(), size, origin, &mut write_region) == 0 {
                let _ = SetConsoleCursorPosition(h, saved);
                return Ok(false);
            }
            i = end;
        }
        let _ = SetConsoleCursorPosition(h, saved);
    }
    Ok(true)
}
