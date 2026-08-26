import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  debounce,
  extractFileName,
  getFileExtension,
  isSupportedFile,
  showStatus,
  typeIcon,
  IMAGE_EXTENSIONS,
} from "./utils";

describe("debounce", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("在延迟到达后执行目标函数", () => {
    const fn = vi.fn();
    const debounced = debounce(fn, 100);

    debounced();
    expect(fn).not.toHaveBeenCalled();

    vi.advanceTimersByTime(100);
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("传递参数给目标函数", () => {
    const fn = vi.fn();
    const debounced = debounce(fn, 50);

    debounced("a", 1);
    vi.advanceTimersByTime(50);

    expect(fn).toHaveBeenCalledWith("a", 1);
  });

  it("连续调用会取消前一次未执行的调用，仅最后一次生效", () => {
    const fn = vi.fn();
    const debounced = debounce(fn, 100);

    debounced("first");
    vi.advanceTimersByTime(50);
    debounced("second");
    vi.advanceTimersByTime(50);
    // 第一次已被取消（定时器重置），仍未执行
    expect(fn).not.toHaveBeenCalled();

    vi.advanceTimersByTime(50);
    expect(fn).toHaveBeenCalledTimes(1);
    expect(fn).toHaveBeenCalledWith("second");
  });

  it("延迟未到时不执行", () => {
    const fn = vi.fn();
    const debounced = debounce(fn, 100);

    debounced();
    vi.advanceTimersByTime(99);
    expect(fn).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1);
    expect(fn).toHaveBeenCalledTimes(1);
  });
});

describe("extractFileName", () => {
  it("从 Windows 路径提取文件名", () => {
    expect(extractFileName("C:\\Users\\test\\image.jpg")).toBe("image.jpg");
  });

  it("从 Unix 路径提取文件名", () => {
    expect(extractFileName("/home/user/wallpaper.png")).toBe("wallpaper.png");
  });

  it("对不含路径分隔符的字符串原样返回", () => {
    expect(extractFileName("file.mp4")).toBe("file.mp4");
  });

  it("对不含扩展名的文件名原样返回", () => {
    expect(extractFileName("README")).toBe("README");
  });

  it("对混合分隔符路径取最后一段", () => {
    expect(extractFileName("C:/dev\\pics/anim.gif")).toBe("anim.gif");
  });

  it("对空字符串返回空字符串", () => {
    expect(extractFileName("")).toBe("");
  });
});

describe("getFileExtension", () => {
  it("返回小写扩展名（jpg）", () => {
    expect(getFileExtension("image.JPG")).toBe("jpg");
  });

  it("返回 mp4 扩展名（大写转小写）", () => {
    expect(getFileExtension("clip.MP4")).toBe("mp4");
  });

  it("返回 gif 扩展名", () => {
    expect(getFileExtension("anim.gif")).toBe("gif");
  });

  it("对无扩展名文件返回空字符串", () => {
    expect(getFileExtension("noext")).toBe("");
  });

  it("对多扩展名文件返回最后一段扩展名", () => {
    expect(getFileExtension("archive.tar.gz")).toBe("gz");
  });

  it("从 Unix 路径提取扩展名", () => {
    expect(getFileExtension("/var/media/video.webm")).toBe("webm");
  });

  it("从 Windows 路径提取扩展名", () => {
    expect(getFileExtension("D:\\media\\movie.mkv")).toBe("mkv");
  });

  it("对点文件返回点后的部分", () => {
    expect(getFileExtension(".gitignore")).toBe("gitignore");
  });
});

describe("typeIcon", () => {
  it("Image 类型返回图片图标", () => {
    expect(typeIcon("Image")).toBe("🖼");
  });

  it("Video 类型返回视频图标", () => {
    expect(typeIcon("Video")).toBe("🎬");
  });

  it("Gif 类型返回胶片图标", () => {
    expect(typeIcon("Gif")).toBe("🎞");
  });

  it("Web 类型返回地球图标", () => {
    expect(typeIcon("Web")).toBe("🌐");
  });

  it("未知类型返回默认图标", () => {
    expect(typeIcon("Unknown")).toBe("📄");
  });

  it("空字符串返回默认图标", () => {
    expect(typeIcon("")).toBe("📄");
  });
});

describe("isSupportedFile", () => {
  it("接受图片扩展名（jpg/jpeg/png/bmp/webp）", () => {
    expect(isSupportedFile("C:/pics/photo.jpg")).toBe(true);
    expect(isSupportedFile("C:/pics/photo.JPEG")).toBe(true);
    expect(isSupportedFile("C:/pics/anim.PNG")).toBe(true);
    expect(isSupportedFile("C:/pics/wallpaper.bmp")).toBe(true);
    expect(isSupportedFile("C:/pics/art.webp")).toBe(true);
  });

  it("接受视频扩展名（mp4/avi/mkv/webm/mov）", () => {
    expect(isSupportedFile("/videos/clip.mp4")).toBe(true);
    expect(isSupportedFile("/videos/clip.AVI")).toBe(true);
    expect(isSupportedFile("/videos/movie.mkv")).toBe(true);
    expect(isSupportedFile("/videos/clip.webm")).toBe(true);
    expect(isSupportedFile("/videos/clip.mov")).toBe(true);
  });

  it("接受 GIF 扩展名", () => {
    expect(isSupportedFile("/gifs/anim.gif")).toBe(true);
    expect(isSupportedFile("/gifs/anim.GIF")).toBe(true);
  });

  it("FE-010: 接受 HTML/HTM 扩展名（Web 类型壁纸）", () => {
    expect(isSupportedFile("/web/page.html")).toBe(true);
    expect(isSupportedFile("/web/page.HTM")).toBe(true);
    expect(isSupportedFile("/web/index.html")).toBe(true);
  });

  it("拒绝不支持的扩展名", () => {
    expect(isSupportedFile("/docs/readme.txt")).toBe(false);
    expect(isSupportedFile("/music/song.mp3")).toBe(false);
    expect(isSupportedFile("/archives/backup.zip")).toBe(false);
    expect(isSupportedFile("/docs/document.pdf")).toBe(false);
  });

  it("拒绝无扩展名文件", () => {
    expect(isSupportedFile("/noext/README")).toBe(false);
    expect(isSupportedFile("noext")).toBe(false);
  });

  it("拒绝空字符串", () => {
    expect(isSupportedFile("")).toBe(false);
  });

  it("扩展名匹配不区分大小写", () => {
    expect(isSupportedFile("C:/PICS/PHOTO.JPG")).toBe(true);
    expect(isSupportedFile("C:/PICS/PHOTO.Mp4")).toBe(true);
    expect(isSupportedFile("C:/PICS/PHOTO.HTML")).toBe(true);
  });
});

describe("扩展名常量集", () => {
  it("IMAGE_EXTENSIONS 包含 5 个图片扩展名", () => {
    expect(IMAGE_EXTENSIONS.size).toBe(5);
    expect(IMAGE_EXTENSIONS.has("jpg")).toBe(true);
    expect(IMAGE_EXTENSIONS.has("jpeg")).toBe(true);
    expect(IMAGE_EXTENSIONS.has("png")).toBe(true);
    expect(IMAGE_EXTENSIONS.has("bmp")).toBe(true);
    expect(IMAGE_EXTENSIONS.has("webp")).toBe(true);
  });
});

describe("showStatus", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    document.body.innerHTML = "";
  });

  it("创建 .status-message 元素并设置文本和类型类名", () => {
    showStatus("操作成功", "success");
    const el = document.querySelector(".status-message") as HTMLDivElement;
    expect(el).not.toBeNull();
    expect(el.textContent).toBe("操作成功");
    expect(el.className).toBe("status-message success");
    expect(el.style.opacity).toBe("1");
  });

  it("复用已存在的 .status-message 元素", () => {
    const existing = document.createElement("div");
    existing.className = "status-message";
    document.body.appendChild(existing);

    showStatus("错误信息", "error");
    const el = document.querySelector(".status-message") as HTMLDivElement;
    expect(el).toBe(existing);
    expect(el.textContent).toBe("错误信息");
    expect(el.className).toBe("status-message error");
  });

  it("3 秒后设置 opacity 为 0", () => {
    showStatus("提示", "info");
    const el = document.querySelector(".status-message") as HTMLDivElement;
    expect(el.style.opacity).toBe("1");
    vi.advanceTimersByTime(3000);
    expect(el.style.opacity).toBe("0");
  });

  it("连续调用会清除前一次的定时器", () => {
    showStatus("第一次", "info");
    const el = document.querySelector(".status-message") as HTMLDivElement;
    vi.advanceTimersByTime(2000);
    showStatus("第二次", "success");
    // 3000ms 后应仍可见（因为第二次调用重置了定时器）
    vi.advanceTimersByTime(1000);
    expect(el.style.opacity).toBe("1");
    // 再过 2000ms（总计第二次后 3000ms）才隐藏
    vi.advanceTimersByTime(2000);
    expect(el.style.opacity).toBe("0");
  });

  it("设置 aria 属性确保无障碍访问", () => {
    showStatus("通知", "info");
    const el = document.querySelector(".status-message") as HTMLDivElement;
    expect(el.getAttribute("role")).toBe("status");
    expect(el.getAttribute("aria-live")).toBe("polite");
    expect(el.getAttribute("aria-atomic")).toBe("true");
  });
});