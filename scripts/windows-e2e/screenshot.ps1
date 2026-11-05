# Saves a screenshot of the runner's desktop (the client's resident window, which the account's process shows there).
param([Parameter(Mandatory)][string]$Path)
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
try {
  $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bmp = [System.Drawing.Bitmap]::new($b.Width, $b.Height)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
  $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
  "screenshot $Path ($($b.Width)x$($b.Height))"
} catch { "no screenshot: $($_.Exception.Message)" }
