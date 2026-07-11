document.documentElement.classList.add('enhanced');

enhanceProjectSearch();
enhanceProjectRegistration();

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

function enhanceProjectRegistration() {
  const form = document.querySelector('[data-project-registration]');
  if (!(form instanceof HTMLFormElement)) return;
  form.addEventListener('submit', (event) => submitProjectRegistration(event, form));
}

async function submitProjectRegistration(event, form) {
  if (!form.reportValidity()) return;
  event.preventDefault();
  const submit = form.querySelector('button[type="submit"]');
  const status = form.querySelector('[role="status"]');
  setRegistrationBusy(submit, status, true);
  try {
    const body = new FormData(form);
    const csrfToken = String(body.get('csrfToken') || '');
    const response = await fetch(form.action, {
      method: 'POST',
      body,
      credentials: 'same-origin',
      headers: {
        'X-Observatory-CSRF': csrfToken,
        'X-Observatory-Enhanced': 'fetch',
      },
    });
    if (response.ok && response.redirected) {
      window.location.assign(response.url);
      return;
    }
    await showRegistrationError(response, status);
  } catch {
    setStatus(status, 'The Observatory daemon could not be reached. Your registration was not confirmed.');
  } finally {
    setRegistrationBusy(submit, status, false);
  }
}

function setRegistrationBusy(submit, status, busy) {
  if (submit instanceof HTMLButtonElement) submit.disabled = busy;
  if (busy) setStatus(status, 'Registering through Observatory…');
}

async function showRegistrationError(response, status) {
  const documentText = await response.text();
  const parsed = new DOMParser().parseFromString(documentText, 'text/html');
  const message = parsed.querySelector('.lede')?.textContent?.trim();
  setStatus(status, message || `Registration failed (${response.status}).`);
}

function setStatus(status, message) {
  if (status) status.textContent = message;
}
