"""Account / Proton sign-in page."""

from __future__ import annotations

import asyncio
import time

from nicegui import app, ui
from nicegui.client import Client

from neutronsync_web import ctx
from neutronsync_web.api import ApiError, SignInStep
from neutronsync_web.auth import FAIL_DELAY, client_address
from neutronsync_web.pages.layout import page_shell, require_session

_FORMS: dict[str, dict] = {}


def _form_for_client() -> dict:
    """Per-browser-client Proton sign-in draft (mirrors settings drafts)."""
    try:
        cid = str(ui.context.client.id)
    except Exception:
        cid = "_anon"
    form = _FORMS.get(cid)
    if form is None:
        form = {
            "step": "login",
            "username": "",
            "totp_code": "",
            "mailbox_password": "",
            "hv_token": None,
            "hv_type": None,
            "hv_url": None,
            "error": None,
            "signing_in": False,
        }
        _FORMS[cid] = form
    return form


def _err(exc: Exception) -> None:
    ui.notify(exc.message if isinstance(exc, ApiError) else str(exc), type="negative")


def render() -> None:
    form = _form_for_client()

    def body() -> None:
        if ctx.get_auth().mode == "password":
            password_form()

        @ui.refreshable
        def form_ui() -> None:
            live = ctx.get_live()
            env = live.state.envelope
            account = env.state.account if env else None
            signed_in = bool(account and account.checked and account.signed_in)

            if form["step"] == "done" or (signed_in and form["step"] in ("login", "login_hv")):
                name = (account.account if account else "") or "Signed in"
                with ui.element("div"):
                    ui.label(name).classes("text-h6")
                    ui.label("Signed in").classes("ns-muted")
                    with ui.row().classes("q-mt-md"):

                        def confirm_out() -> None:
                            with ui.dialog() as d, ui.card():
                                ui.label("Sign out of Proton?").classes("text-h6")
                                with ui.row().classes("justify-end w-full"):
                                    ui.button("Cancel", on_click=d.close).props("flat")

                                    async def do_out() -> None:
                                        d.close()
                                        try:
                                            await ctx.get_api().logout()
                                            form["step"] = "login"
                                            form_ui.refresh()
                                        except Exception as exc:
                                            _err(exc)

                                    ui.button("Sign out", on_click=do_out).props("color=negative")
                            d.open()

                        ui.button("Sign out", on_click=confirm_out).props("color=negative")
                return

            if form["error"]:
                ui.label(form["error"]).classes("ns-err")

            if form["step"] == "need_2fa":
                ui.label("Two-factor code").classes("ns-auth-title")
                ui.label("Enter the 6-digit code from your authenticator app.").classes("ns-muted")
                code = (
                    ui.input("6-digit code", value=form["totp_code"])
                    .props("maxlength=6")
                    .classes("w-full")
                )
                code.on_value_change(lambda e: form.update(totp_code=e.value or ""))

                async def send_2fa() -> None:
                    if form["signing_in"]:
                        return
                    form["signing_in"] = True
                    form_ui.refresh()
                    try:
                        value = (code.value or form["totp_code"] or "").strip()
                        step = await ctx.get_api().submit_2fa(value)
                        form["totp_code"] = ""
                        code.value = ""
                        _apply_step(step)
                    except ApiError as exc:
                        form["error"] = exc.message
                        form["totp_code"] = ""
                        code.value = ""
                    except Exception as exc:
                        _err(exc)
                    finally:
                        form["signing_in"] = False
                        form_ui.refresh()

                btn = (
                    ui.button("Continue", on_click=send_2fa)
                    .props("color=primary")
                    .classes("w-full")
                )
                if form["signing_in"]:
                    btn.props("disable")
                    with ui.row().classes("items-center gap-2 q-mt-sm"):
                        ui.spinner(size="sm")
                        ui.label("Signing in…")
                return

            if form["step"] == "need_mailbox_password":
                ui.label("Mailbox password").classes("ns-auth-title")
                ui.label("Your account uses a second password to unlock its data.").classes(
                    "ns-muted"
                )
                pw = ui.input(
                    "Mailbox password",
                    value=form["mailbox_password"],
                    password=True,
                    password_toggle_button=True,
                ).classes("w-full")
                pw.on_value_change(lambda e: form.update(mailbox_password=e.value or ""))

                async def send_mb() -> None:
                    if form["signing_in"]:
                        return
                    form["signing_in"] = True
                    form_ui.refresh()
                    try:
                        value = pw.value or form["mailbox_password"] or ""
                        step = await ctx.get_api().submit_mailbox_password(value)
                        form["mailbox_password"] = ""
                        pw.value = ""
                        _apply_step(step)
                    except ApiError as exc:
                        form["error"] = exc.message
                        form["mailbox_password"] = ""
                        pw.value = ""
                    except Exception as exc:
                        _err(exc)
                    finally:
                        form["signing_in"] = False
                        form_ui.refresh()

                btn = (
                    ui.button("Continue", on_click=send_mb).props("color=primary").classes("w-full")
                )
                if form["signing_in"]:
                    btn.props("disable")
                    with ui.row().classes("items-center gap-2 q-mt-sm"):
                        ui.spinner(size="sm")
                        ui.label("Signing in…")
                return

            if form["step"] == "need_human_verification":
                ui.label("Human verification").classes("ns-auth-title")
                url = form["hv_url"] or "#"
                ui.label("Proton needs a human check. Open the page, complete it, then continue.")
                ui.button(
                    "Open verification",
                    on_click=lambda: ui.navigate.to(url, new_tab=True),
                ).props("outline")

                def done_hv() -> None:
                    form["step"] = "login_hv"
                    form_ui.refresh()

                ui.button("I've completed it", on_click=done_hv).props("color=primary")
                return

            ui.label("Sign in to Proton").classes("ns-auth-title")
            ui.label(
                "Your password goes straight to Proton and is never stored. "
                "NeutronSync keeps only the encrypted session."
            ).classes("ns-muted")
            user = ui.input("Email or username", value=form["username"]).classes("w-full")
            user.on_value_change(lambda e: form.update(username=e.value or ""))
            pw = ui.input("Password", password=True, password_toggle_button=True).classes("w-full")

            async def send_login() -> None:
                if form["signing_in"]:
                    return
                form["signing_in"] = True
                form_ui.refresh()
                form["username"] = (user.value or form["username"] or "").strip()
                password = pw.value or ""
                try:
                    kwargs = {}
                    if form["step"] == "login_hv" and form["hv_token"]:
                        kwargs["hv_token"] = form["hv_token"]
                        kwargs["hv_type"] = form["hv_type"] or "captcha"
                    step = await ctx.get_api().login(form["username"], password, **kwargs)
                    form["error"] = None
                    _apply_step(step)
                except ApiError as exc:
                    form["error"] = exc.message
                except Exception as exc:
                    _err(exc)
                finally:
                    form["signing_in"] = False
                    form_ui.refresh()

            btn = ui.button("Sign in", on_click=send_login).props("color=primary").classes("w-full")
            if form["signing_in"]:
                btn.props("disable")
                with ui.row().classes("items-center gap-2 q-mt-sm"):
                    ui.spinner(size="sm")
                    ui.label("Signing in…")

        def _apply_step(step: SignInStep) -> None:
            form["step"] = step.step or "done"
            if step.step == "need_human_verification":
                form["hv_url"] = step.url
                form["hv_token"] = step.token
                form["hv_type"] = (step.methods or ["captcha"])[0]
            if step.step == "done":
                form["step"] = "done"
            form_ui.refresh()

        with ui.element("div").classes("ns-auth"):
            form_ui()

    page_shell("Account", body, live_body=False)


def password_form() -> None:
    """Change the web password without changing Proton credentials."""
    auth = ctx.get_auth()
    with ui.column().classes("ns-auth"):
        ui.label("Change web password").classes("ns-auth-title")
        current = ui.input("Current password", password=True, password_toggle_button=True)
        password = ui.input("New password", password=True, password_toggle_button=True)
        confirmation = ui.input("Confirm new password", password=True, password_toggle_button=True)
        message = ui.label("").classes("ns-err")

        async def submit() -> None:
            if not require_session():
                ui.navigate.to("/login")
                return
            addr = client_address(ui.context.client.request)
            if auth.rate_limited(addr):
                message.set_text("Too many attempts. Try again in a few minutes.")
                return
            try:
                auth.change_password(
                    current.value or "", password.value or "", confirmation.value or ""
                )
            except ValueError as exc:
                auth.record_failure(addr)
                message.set_text(str(exc))
                await asyncio.sleep(FAIL_DELAY)
                return
            finally:
                current.value = ""
                password.value = ""
                confirmation.value = ""
            auth.clear_failures(addr)
            app.storage.user["session_version"] = auth.session_version
            app.storage.user["signed_at"] = time.time()
            # Remove old controls immediately, including their server-side handlers.
            # Reloading an old browser session then checks the new session version.
            for client in list(Client.instances.values()):
                if client is not ui.context.client:
                    client.remove_all_elements()
                    client.run_javascript("window.location.replace('/login')")
            message.set_text("Web password changed. Other sessions are signed out.")

        ui.button("Change web password", on_click=submit).props("color=primary")
