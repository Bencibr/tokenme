/**
 * The project's self-description, in one file: where releases live, where the
 * update manifest is fetched from, and how a user reaches the author.
 *
 * 发布到 GitHub 时只需要改这一处：GITHUB_REPO。UPDATE_MANIFEST_URL 指向一个
 * `{"version":"x.y.z","url":"…"}` 的 JSON（scripts/release.sh 会在打 tag 时
 * 自动重写并提交）；国内用户访问 GitHub 慢的话，把同一份 latest.json 丢到
 * 七牛等免费 OSS，把 URL 换成 OSS 直链即可，格式不变。
 */
export const GITHUB_REPO = "sp/tokenme";
export const RELEASE_PAGE_URL = `https://github.com/${GITHUB_REPO}/releases/latest`;
export const UPDATE_MANIFEST_URL = `https://raw.githubusercontent.com/${GITHUB_REPO}/main/latest.json`;
export const CONTACT_EMAIL = "benci@oksu.club";
