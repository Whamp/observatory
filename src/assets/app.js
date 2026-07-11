document.documentElement.classList.add('enhanced');

enhanceProjectSearch();
enhanceProjectForms();

function enhanceProjectSearch() {
  const search = document.querySelector('#project-search');
  const rows = [...document.querySelectorAll('[data-project-row]')];
  const count = document.querySelector('[data-project-count]');
  if (!search || rows.length === 0) return;
  search.addEventListener('input', () => filterProjectRows(search, rows, count));
}

function filterProjectRows(search, rows, count) {
  const query = search.value.trim().toLocaleLowerCase();
  let visible = 0;
  for (const row of rows) {
    const matches = row.textContent.toLocaleLowerCase().includes(query);
    row.hidden = !matches;
    visible += Number(matches);
  }
  if (count) count.textContent = `${visible} shown`;
}

function enhanceProjectForms() {
  const selector = [
    '[data-project-registration]',
    '[data-project-update]',
    '[data-project-tombstone]',
  ].join(',');
  for (const form of document.querySelectorAll(selector)) {
    if (!(form instanceof HTMLFormElement)) continue;
    form.addEventListener('submit', (event) => submitProjectForm(event, form));
  }
}

async function submitProjectForm(event, form) {
  if (!form.reportValidity()) return;
  event.preventDefault();
  const submit = form.querySelector('button[type="submit"]');
  const status = form.querySelector('[role="status"]');
  setFormBusy(submit, status, true);
  try {
    const formData = new FormData(form);
    const body = new URLSearchParams();
    for (const [name, value] of formData) body.append(name, String(value));
    const csrfToken = String(formData.get('csrfToken') || '');
    const response = await fetch(form.action, {
      method: 'POST',
      body,
      credentials: 'same-origin',
      headers: {
        'X-Observatory-CSRF': csrfToken,
        'X-Observatory-Enhanced': 'fetch',
      },
    });
    if (response.redirected) {
      window.location.assign(response.url);
      return;
    }
    await showFormError(response, status);
  } catch {
    setStatus(status, 'The Observatory daemon could not be reached. This change was not confirmed.');
  } finally {
    setFormBusy(submit, status, false);
  }
}

function setFormBusy(submit, status, busy) {
  if (submit instanceof HTMLButtonElement) submit.disabled = busy;
  if (busy) setStatus(status, 'Submitting through Observatory…');
}

async function showFormError(response, status) {
  const documentText = await response.text();
  const parsed = new DOMParser().parseFromString(documentText, 'text/html');
  const message = parsed.querySelector('.lede')?.textContent?.trim();
  setStatus(status, message || `Project change failed (${response.status}).`);
}

function setStatus(status, message) {
  if (status) status.textContent = message;
}
