use crate::project::{Project, ProjectList, ProjectTombstonePreview};

pub fn index(projects: &ProjectList, build_id: &str, query: &str) -> String {
    let mut body = String::new();
    body.push_str("<header class=\"masthead\"><div><p class=\"eyebrow\">Tailnet catalogue</p><h1>Observatory Projects</h1><p class=\"lede\">The known entry point for browser-based agent work.</p></div><a class=\"primary\" href=\"/ui/projects/new/\">Register Project</a></header>");
    body.push_str("<form class=\"search\" action=\"/ui/\" method=\"get\"><label for=\"project-search\">Search Projects</label><div><input id=\"project-search\" name=\"query\" type=\"search\" value=\"");
    body.push_str(&attribute_escape(query));
    body.push_str("\" placeholder=\"Title, directory, or key\"><button type=\"submit\">Search</button></div></form>");
    body.push_str("<section aria-labelledby=\"project-list-title\"><div class=\"section-heading\"><h2 id=\"project-list-title\">Project ledger</h2><span data-project-count>");
    body.push_str(&projects.items().len().to_string());
    body.push_str(" shown</span></div>");
    if projects.items().is_empty() {
        body.push_str("<div class=\"empty\"><h3>No projects yet</h3><p>Register a host-local directory to establish its stable Observatory identity.</p></div>");
    } else {
        body.push_str("<ol class=\"project-list\">");
        for project in projects.items() {
            body.push_str("<li data-project-row><a href=\"");
            body.push_str(&attribute_escape(project.detail_url()));
            body.push_str("\"><span class=\"project-title\">");
            body.push_str(&text_escape(project.title()));
            body.push_str("</span><code>");
            body.push_str(&text_escape(project.key()));
            body.push_str("</code><span class=\"state live\">live</span></a></li>");
        }
        body.push_str("</ol>");
    }
    body.push_str("</section>");
    shell("Projects", &body, build_id)
}

pub fn register_form(csrf_token: &str, idempotency_key: &str, build_id: &str) -> String {
    let mut body = String::new();
    body.push_str("<nav><a href=\"/ui/\">← Project ledger</a></nav><header><p class=\"eyebrow\">New identity</p><h1>Register Project</h1><p class=\"lede\">Registration records one existing host-local directory. It does not scan or copy its contents.</p></header>");
    body.push_str("<form class=\"registration\" action=\"/ui/projects/\" method=\"post\" data-project-registration><input type=\"hidden\" name=\"csrfToken\" value=\"");
    body.push_str(&attribute_escape(csrf_token));
    body.push_str("\"><input type=\"hidden\" name=\"idempotencyKey\" value=\"");
    body.push_str(&attribute_escape(idempotency_key));
    body.push_str("\"><label for=\"project-path\">Directory path <span>required</span></label><input id=\"project-path\" name=\"path\" type=\"text\" required autocomplete=\"off\" placeholder=\"/home/agent/projects/example\"><label for=\"project-title\">Title <span>optional</span></label><input id=\"project-title\" name=\"title\" type=\"text\" autocomplete=\"off\"><label for=\"project-slug\">Route slug <span>optional</span></label><input id=\"project-slug\" name=\"slug\" type=\"text\" autocomplete=\"off\" pattern=\"[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?\"><p class=\"form-status\" role=\"status\" aria-live=\"polite\"></p><button class=\"primary\" type=\"submit\">Register Project</button></form>");
    shell("Register Project", &body, build_id)
}

pub fn project_detail(
    project: &Project,
    csrf_token: &str,
    idempotency_key: &str,
    build_id: &str,
) -> String {
    let mut body = String::new();
    body.push_str("<nav><a href=\"/ui/\">← Project ledger</a></nav><header><p class=\"eyebrow\">Live Project</p><h1>");
    body.push_str(&text_escape(project.title()));
    body.push_str("</h1><p class=\"lede\"><code>");
    body.push_str(&text_escape(project.key()));
    body.push_str(
        "</code></p></header><dl class=\"facts\"><div><dt>Canonical directory</dt><dd><code>",
    );
    body.push_str(&text_escape(project.canonical_directory()));
    body.push_str("</code></dd></div><div><dt>Project ID</dt><dd><code>");
    body.push_str(&text_escape(project.id()));
    body.push_str("</code></dd></div></dl><section class=\"project-controls\" aria-labelledby=\"project-settings\"><div class=\"section-heading\"><h2 id=\"project-settings\">Project settings</h2><span>Presentation only</span></div><form class=\"registration compact\" action=\"");
    body.push_str(&attribute_escape(&format!(
        "/ui/projects/{}/update/",
        project.key()
    )));
    body.push_str(
        "\" method=\"post\" data-project-update><input type=\"hidden\" name=\"csrfToken\" value=\"",
    );
    body.push_str(&attribute_escape(csrf_token));
    body.push_str("\"><input type=\"hidden\" name=\"idempotencyKey\" value=\"");
    body.push_str(&attribute_escape(idempotency_key));
    body.push_str("\"><input type=\"hidden\" name=\"ifMatch\" value=\"");
    body.push_str(&attribute_escape(&project.etag()));
    body.push_str("\"><label for=\"project-title\">Title</label><input id=\"project-title\" name=\"title\" type=\"text\" required value=\"");
    body.push_str(&attribute_escape(project.title()));
    body.push_str("\"><label for=\"project-slug\">Route slug</label><input id=\"project-slug\" name=\"slug\" type=\"text\" required pattern=\"[a-z0-9](?:[a-z0-9-]{0,46}[a-z0-9])?\" value=\"");
    body.push_str(&attribute_escape(project.slug()));
    body.push_str("\"><p class=\"form-status\" role=\"status\" aria-live=\"polite\"></p><button type=\"submit\">Update Project</button></form><a class=\"danger-link\" href=\"");
    body.push_str(&attribute_escape(&format!(
        "/ui/projects/{}/tombstone/",
        project.key()
    )));
    body.push_str("\">Tombstone Project</a></section><section><div class=\"section-heading\"><h2>Entries</h2><span>0 shown</span></div><div class=\"empty\"><h3>No Entries yet</h3><p>Published Artifacts and registered Services will appear here.</p></div></section>");
    shell(project.title(), &body, build_id)
}

pub fn tombstone_review(
    preview: &ProjectTombstonePreview,
    csrf_token: &str,
    idempotency_key: &str,
    build_id: &str,
) -> String {
    let project = preview.project();
    let mut body = String::new();
    body.push_str("<nav><a href=\"");
    body.push_str(&attribute_escape(&format!(
        "/ui/projects/{}/",
        project.key()
    )));
    body.push_str("\">← Project detail</a></nav><header><p class=\"eyebrow danger\">Permanent identity retirement</p><h1>Tombstone Project</h1><p class=\"lede\">The Project identity becomes permanently gone. Associated Artifacts remain associated and continue their own lifecycle.</p></header><div class=\"confirmation-facts\"><strong>");
    body.push_str(&preview.live_services().to_string());
    body.push_str(" live Services</strong><strong>");
    body.push_str(&preview.associated_artifacts().to_string());
    body.push_str(" associated Artifacts</strong><strong>");
    body.push_str(&preview.active_operations().to_string());
    body.push_str(
        " active operations</strong></div><form class=\"registration danger-zone\" action=\"",
    );
    body.push_str(&attribute_escape(&format!(
        "/ui/projects/{}/tombstone/",
        project.key()
    )));
    body.push_str("\" method=\"post\" data-project-tombstone><input type=\"hidden\" name=\"csrfToken\" value=\"");
    body.push_str(&attribute_escape(csrf_token));
    body.push_str("\"><input type=\"hidden\" name=\"idempotencyKey\" value=\"");
    body.push_str(&attribute_escape(idempotency_key));
    body.push_str("\"><input type=\"hidden\" name=\"ifMatch\" value=\"");
    body.push_str(&attribute_escape(&project.etag()));
    body.push_str("\"><label for=\"project-confirmation\">Type the exact Project key <code>");
    body.push_str(&text_escape(project.key()));
    body.push_str("</code></label><input id=\"project-confirmation\" name=\"confirmation\" type=\"text\" required autocomplete=\"off\"><p class=\"form-status\" role=\"status\" aria-live=\"polite\"></p><button class=\"danger-button\" type=\"submit\">Tombstone Project</button></form>");
    shell("Tombstone Project", &body, build_id)
}

pub fn error(title: &str, message: &str, build_id: &str) -> String {
    let mut body = String::new();
    body.push_str("<nav><a href=\"/ui/\">← Project ledger</a></nav><header><p class=\"eyebrow\">Request rejected</p><h1>");
    body.push_str(&text_escape(title));
    body.push_str("</h1><p class=\"lede\">");
    body.push_str(&text_escape(message));
    body.push_str("</p></header>");
    shell(title, &body, build_id)
}

fn shell(title: &str, body: &str, build_id: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"color-scheme\" content=\"dark\"><title>{} · Observatory</title><link rel=\"icon\" href=\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Ccircle cx='16' cy='16' r='12' fill='%23f0bc5b'/%3E%3Ccircle cx='16' cy='16' r='5' fill='%2307100d'/%3E%3C/svg%3E\"><link rel=\"stylesheet\" href=\"/_static/{}/app.css\"><script type=\"module\" src=\"/_static/{}/app.js\"></script></head><body><a class=\"skip\" href=\"#content\">Skip to content</a><main id=\"content\">{}</main></body></html>",
        text_escape(title),
        attribute_escape(build_id),
        attribute_escape(build_id),
        body
    )
}

fn text_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn attribute_escape(value: &str) -> String {
    text_escape(value)
}
