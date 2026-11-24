pub const FURL_MARKER: &str = "<<<FURL>>>";
pub const IMAGE_MARKER: &str = "<<<IMAGE>>>";

/// Pasteboard raster classes the AppleScript coerces to, in probe order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RasterClass {
    Png,
    Tiff,
    Jpeg,
}

impl RasterClass {
    pub const ALL: [Self; 3] = [Self::Png, Self::Tiff, Self::Jpeg];

    /// The `«class PNGf»` code, also what the script prints on stdout.
    pub fn code(self) -> &'static str {
        match self {
            Self::Png => "PNGf",
            Self::Tiff => "TIFF",
            Self::Jpeg => "JPEG",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Tiff => "image/tiff",
            Self::Jpeg => "image/jpeg",
        }
    }
}

impl std::str::FromStr for RasterClass {
    type Err = ();

    fn from_str(code: &str) -> Result<Self, ()> {
        Self::ALL
            .into_iter()
            .find(|class| class.code() == code)
            .ok_or(())
    }
}

/// Private scratch directory for one AppleScript raster hand-off.
pub struct ProbeTemps(super::super::ScratchDir);

impl ProbeTemps {
    pub fn new() -> std::io::Result<Self> {
        super::super::ScratchDir::new().map(Self)
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.0.path().join("probe")
    }
}

pub(super) fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Nested `try` chain coercing each class in order into this probe's file; the code of the one that wrote lands in the caller's `imageOut`. Guillemets are required for `«class …»`.
pub fn raster_try_chain(temps: &ProbeTemps) -> String {
    let path = applescript_string(&temps.path().display().to_string());
    let mut script = String::new();
    for class in RasterClass::ALL {
        script.push_str(&format!(
            "try\n\
             set imgData to the clipboard as \u{00AB}class {code}\u{00BB}\n\
             set filePath to POSIX file {path} as text\n\
             set fRef to open for access file filePath with write permission\n\
             set eof of fRef to 0\n\
             write imgData to fRef\n\
             close access fRef\n\
             set imageOut to \"{code}\"\n\
             on error\n",
            code = class.code(),
        ));
    }
    for _ in RasterClass::ALL {
        script.push_str("end try\n");
    }
    script
}

/// Prints the class code that wrote, else `none`.
pub fn image_osascript(temps: &ProbeTemps) -> String {
    format!(
        "set imageOut to \"none\"\n{}return imageOut",
        raster_try_chain(temps)
    )
}

/// File URLs first, rasters only when none are present; without `temps` the raster half is skipped so a Finder paste still works. Stdout follows [`parse_attachments_output`].
pub fn attachments_osascript(temps: Option<&ProbeTemps>) -> String {
    let image_probe = temps.map_or_else(String::new, |temps| {
        format!(
            "if furlOut is \"none\" then\n{}end if\n",
            raster_try_chain(temps)
        )
    });
    format!(
        "set furlOut to \"none\"\n\
         try\n\
         set urlList to the clipboard as list\n\
         set out to \"\"\n\
         repeat with u in urlList\n\
         try\n\
         set itemRef to contents of u as \u{00AB}class furl\u{00BB}\n\
         set out to out & POSIX path of itemRef & \"\\n\"\n\
         end try\n\
         end repeat\n\
         if out is not \"\" then\n\
         set furlOut to out\n\
         else\n\
         try\n\
         set urlRef to the clipboard as \u{00AB}class furl\u{00BB}\n\
         set furlOut to POSIX path of urlRef\n\
         on error\n\
         end try\n\
         end if\n\
         on error\n\
         try\n\
         set urlRef to the clipboard as \u{00AB}class furl\u{00BB}\n\
         set furlOut to POSIX path of urlRef\n\
         on error\n\
         end try\n\
         end try\n\
         set imageOut to \"NONE\"\n\
         {image_probe}\
         return \"{furl_marker}\" & linefeed & furlOut & linefeed & \"{image_marker}\" & linefeed & \"IMAGE:\" & imageOut",
        furl_marker = FURL_MARKER,
        image_marker = IMAGE_MARKER,
    )
}

/// Parse the stdout payload of the `get_file_urls` AppleScript. The script returns one of: one or more POSIX paths
/// separated by `\n` (success), or; the literal string `"none"` (no file URLs present), or; empty / whitespace-only
/// (degenerate cases that map to `None`).
pub fn parse_osascript_furl_output(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "none" {
        return None;
    }
    Some(trimmed.to_owned())
}

/// Parse stdout from the unified attachments AppleScript. The image section is `NONE` when file URLs were found (image
/// probe skipped) or when no image type is on the pasteboard. When both sections parse successfully, file URLs take
/// precedence over the image class at the [`super::get_attachments`] layer. Image bytes are not read if `file_urls` is `Some`.
pub fn parse_attachments_output(raw: &str) -> (Option<String>, Option<RasterClass>) {
    let (furl_section, image_line) = match raw.split_once(IMAGE_MARKER) {
        Some((before, after)) => (before, Some(after)),
        None => (raw, None),
    };
    let file_urls = if furl_section.contains(FURL_MARKER) {
        let furl_body = furl_section
            .strip_prefix(FURL_MARKER)
            .unwrap_or(furl_section)
            .trim_start_matches(['\n', '\r']);
        parse_osascript_furl_output(furl_body)
    } else {
        None
    };
    let image_class = image_line.and_then(|line| {
        line.trim()
            .strip_prefix("IMAGE:")
            .and_then(|code| code.trim().parse::<RasterClass>().ok())
    });
    (file_urls, image_class)
}

/// The bytes the script wrote for the class it reported; `None` when it wrote nothing.
pub fn read_probe_raster(
    class: RasterClass,
    temps: &ProbeTemps,
) -> std::io::Result<Option<super::ImageData>> {
    let data = std::fs::read(temps.path())?;
    if data.is_empty() {
        return Ok(None);
    }
    Ok(Some(super::ImageData {
        data,
        mime_type: class.mime().to_owned(),
    }))
}

/// The `osascript` half of [`super::get_attachments`]; `run` executes a script and returns its stdout. Without a scratch dir the
/// furl half still answers a file paste; with no file URL either the board is unreadable, not empty, as [`super::get_image`] reports it.
pub fn osascript_attachments(
    temps: std::io::Result<ProbeTemps>,
    run: impl FnOnce(&str) -> std::io::Result<Vec<u8>>,
) -> std::io::Result<(Option<super::ImageData>, Option<String>)> {
    let stdout = run(&attachments_osascript(temps.as_ref().ok()))?;
    let (file_urls, image_class) = parse_attachments_output(&String::from_utf8_lossy(&stdout));
    if file_urls.is_some() {
        return Ok((None, file_urls));
    }
    let temps = temps?;
    let image = match image_class {
        Some(class) => read_probe_raster(class, &temps)?,
        None => None,
    };
    Ok((image, None))
}
